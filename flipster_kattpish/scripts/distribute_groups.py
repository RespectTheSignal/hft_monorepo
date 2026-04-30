#!/usr/bin/env python3
"""Set up 10 Vultr Tokyo instances, each running collector+executor for
its assigned 32-symbol group. All share the same Flipster account
cookie — we partition the universe across IPs so per-IP CF rate-limits
spread out, and one IP's block only takes 32 symbols offline.

Usage:
  distribute_groups.py --setup     # rsync code+cookies+env, build, start each
  distribute_groups.py --status    # health-check all 10
  distribute_groups.py --stop-all
  distribute_groups.py --start-all
"""
import argparse
import json
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

PROJECT = Path(__file__).resolve().parent.parent
GROUPS_FILE = PROJECT / "data" / "symbol_groups.json"
INSTANCES_FILE = PROJECT / "data" / "group_instances.json"
COOKIE_PATH = "/home/gate1/.config/flipster_kattpish/cookies.json"


def load() -> tuple[list, dict]:
    groups = json.loads(GROUPS_FILE.read_text())
    instances = json.loads(INSTANCES_FILE.read_text())
    return groups, instances


def ssh(ip: str, cmd: str, timeout: int = 60) -> tuple[int, str]:
    try:
        p = subprocess.run(
            ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
             "-o", "BatchMode=yes", f"root@{ip}", cmd],
            capture_output=True, text=True, timeout=timeout,
        )
        return p.returncode, p.stdout + p.stderr
    except Exception as e:
        return 1, str(e)


def wait_ssh(ip: str, timeout_s: int = 300) -> bool:
    t0 = time.time()
    while time.time() - t0 < timeout_s:
        rc, out = ssh(ip, "echo ready", timeout=10)
        if rc == 0 and "ready" in out:
            return True
        time.sleep(10)
    return False


def setup_one(group_id: int, ip: str, symbols: list[str]) -> dict:
    """rsync code/cookies/env, write group-specific .env override,
    start collector + executor."""
    print(f"  [g{group_id}] starting setup on {ip}")

    # 1. wait SSH
    if not wait_ssh(ip, timeout_s=300):
        return {"group": group_id, "ip": ip, "status": "ssh_timeout"}

    # 2. rsync cookies + env (snapshot is from earlier, may have stale cookies)
    subprocess.run(
        ["rsync", "-a", COOKIE_PATH,
         f"root@{ip}:/root/.config/flipster_kattpish/cookies.json"],
        capture_output=True, timeout=60,
    )
    subprocess.run(
        ["rsync", "-a", str(PROJECT / ".env"),
         f"root@{ip}:/root/projects/quant/hft_monorepo/flipster_kattpish/.env"],
        capture_output=True, timeout=60,
    )

    # 3. Configure group-specific whitelist + depth subscription
    bases = ",".join(symbols)
    flipster_syms = ",".join(f"{s}USDT.PERP" for s in symbols)
    setup_cmd = f'''
cd /root/projects/quant/hft_monorepo/flipster_kattpish || exit 1

python3 -c "
import re
with open('.env') as f: txt = f.read()
# Disable proxy pool (direct from Tokyo)
txt = re.sub(r'^FLIPSTER_PROXY_POOL=.*$', '# FLIPSTER_PROXY_POOL= (disabled, direct)', txt, flags=re.M)
# Force-set whitelist + depth_symbols for this group
txt = re.sub(r'^GL_WHITELIST=.*$', 'GL_WHITELIST={bases}', txt, flags=re.M)
txt = re.sub(r'^FLIPSTER_DEPTH_SYMBOLS=.*$', 'FLIPSTER_DEPTH_SYMBOLS={flipster_syms}', txt, flags=re.M)
with open('.env','w') as f: f.write(txt)
"

# Stop any pre-existing services from snapshot
./scripts/control.sh gl-stop 2>/dev/null
./scripts/control.sh collector-stop 2>/dev/null
sleep 1

./scripts/control.sh collector-start && sleep 2 && \\
./scripts/control.sh gl-start
sleep 3
ps -ef | grep -E "target/release/(collector|executor)" | grep -v grep | wc -l
'''
    rc, out = ssh(ip, setup_cmd, timeout=60)
    procs = 0
    try:
        procs = int(out.strip().split('\n')[-1])
    except Exception:
        pass
    return {"group": group_id, "ip": ip,
            "status": "ok" if procs >= 2 else f"failed_procs={procs}",
            "procs": procs}


def cmd_setup() -> None:
    groups, instances = load()
    tasks = []
    for g in groups:
        gid = g["group"]
        if str(gid) not in instances:
            print(f"  [g{gid}] skip — no instance")
            continue
        ip = instances[str(gid)]["ip"]
        if not ip:
            print(f"  [g{gid}] skip — no IP")
            continue
        tasks.append((gid, ip, g["symbols"]))

    print(f"=== setting up {len(tasks)} groups in parallel ===")
    with ThreadPoolExecutor(max_workers=len(tasks)) as ex:
        results = list(ex.map(lambda t: setup_one(*t), tasks))

    print("\n=== summary ===")
    for r in results:
        print(f"  g{r['group']:<2d} {r['ip']:18s} {r['status']}")

    # Update gate1 cron to push cookies to all 10
    update_cookie_cron([(r["group"], r["ip"]) for r in results if r["status"] == "ok"])


def update_cookie_cron(group_ips: list[tuple[int, str]]) -> None:
    """Cron: every 5min, rsync cookies to each healthy NRT."""
    p = subprocess.run(["crontab", "-l"], capture_output=True, text=True)
    lines = p.stdout.splitlines() if p.returncode == 0 else []
    new_lines = [l for l in lines if "flipster_kattpish/cookies.json" not in l]
    for gid, ip in group_ips:
        new_lines.append(
            f"*/5 * * * * rsync -a {COOKIE_PATH} root@{ip}:/root/.config/flipster_kattpish/cookies.json >/dev/null 2>&1"
        )
    subprocess.run(["crontab", "-"], input="\n".join(new_lines) + "\n",
                   text=True, capture_output=True)
    print(f"\ncron updated — pushing cookies to {len(group_ips)} hosts")


def cmd_status() -> None:
    groups, instances = load()

    def check(args):
        gid, ip = args
        rc, out = ssh(ip,
            "ps -ef | grep -E 'target/release/(collector|executor)' | grep -v grep | wc -l && "
            "curl -s -o /dev/null -w '%{http_code}' --max-time 5 https://api.flipster.io/api/v2/health",
            timeout=15)
        lines = out.strip().split('\n')
        try:
            procs = int(lines[0])
            health = lines[-1]
        except Exception:
            procs, health = 0, "?"
        return (gid, ip, procs, health)

    tasks = [(g["group"], instances[str(g["group"])]["ip"]) for g in groups
             if str(g["group"]) in instances and instances[str(g["group"])]["ip"]]
    print(f"=== status of {len(tasks)} groups ===")
    print(f"{'g':>3s} {'ip':18s} {'procs':>6s} {'health':>7s} {'symbols (preview)'}")
    with ThreadPoolExecutor(max_workers=len(tasks)) as ex:
        for gid, ip, procs, health in ex.map(check, tasks):
            sym_preview = ",".join(groups[gid-1]["symbols"][:5]) + "..."
            mark = "✓" if procs >= 2 and health == "200" else "✗"
            print(f"  {gid:>2d} {ip:18s} {procs:>6d} {health:>7s} {mark} {sym_preview}")


def cmd_stop_all() -> None:
    _, instances = load()

    def stop(args):
        gid, ip = args
        ssh(ip, "cd /root/projects/quant/hft_monorepo/flipster_kattpish && "
                "./scripts/control.sh gl-stop && ./scripts/control.sh collector-stop",
            timeout=30)
        return gid

    tasks = [(int(gid), v["ip"]) for gid, v in instances.items() if v["ip"]]
    with ThreadPoolExecutor(max_workers=len(tasks)) as ex:
        for gid in ex.map(stop, tasks):
            print(f"  g{gid} stopped")


def cmd_start_all() -> None:
    _, instances = load()

    def start(args):
        gid, ip = args
        ssh(ip, "cd /root/projects/quant/hft_monorepo/flipster_kattpish && "
                "./scripts/control.sh collector-start && sleep 2 && "
                "./scripts/control.sh gl-start",
            timeout=30)
        return gid

    tasks = [(int(gid), v["ip"]) for gid, v in instances.items() if v["ip"]]
    with ThreadPoolExecutor(max_workers=len(tasks)) as ex:
        for gid in ex.map(start, tasks):
            print(f"  g{gid} started")


def main() -> None:
    p = argparse.ArgumentParser()
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--setup", action="store_true")
    g.add_argument("--status", action="store_true")
    g.add_argument("--stop-all", action="store_true")
    g.add_argument("--start-all", action="store_true")
    args = p.parse_args()

    if args.setup:
        cmd_setup()
    elif args.status:
        cmd_status()
    elif args.stop_all:
        cmd_stop_all()
    elif args.start_all:
        cmd_start_all()


if __name__ == "__main__":
    main()
