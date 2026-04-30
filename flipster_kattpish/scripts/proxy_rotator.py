#!/usr/bin/env python3
"""Auto-rotate Vultr proxies when Cloudflare rate-limits them.

Usage:
  proxy_rotator.py --once          # health-check + rotate any blocked, exit
  proxy_rotator.py --watch         # daemon: repeat every 30s
  proxy_rotator.py --status        # print current pool + Vultr inventory
  proxy_rotator.py --rotate-all    # destroy + recreate every proxy

The rotator owns:
  - data/vultr_proxies.json — {ip → {id, region, plan, os_id, created_at}}
  - .env's FLIPSTER_PROXY_POOL line (atomic rewrite on change)
  - scripts/control.sh gl-stop/gl-start (called after pool change so the
    executor reloads the new proxy list)

When a proxy returns 429 from Flipster's /api/v2/health, the rotator:
  1. Marks it dead in-memory.
  2. Calls Vultr DELETE → instance gone (~10s).
  3. POST a new instance with the same region/plan/os and our cloud-init
     script as user_data (tinyproxy auto-installs).
  4. Polls the new IP's :3128 with auth-required CONNECT until it
     answers (provisioning ~120-180s on Vultr Seoul).
  5. Rewrites .env's FLIPSTER_PROXY_POOL atomically.
  6. Bounces the executor (and collector, since it shares the pool via
     env at startup) so they pick up the new pool.

Cooldown: don't rotate the same Vultr instance more than once per 1h —
prevents thrashing if CF starts blocking by /24 instead of /32.
"""
import argparse
import base64
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parent
ENV_FILE = PROJECT / ".env"
STATE_FILE = PROJECT / "data" / "vultr_proxies.json"
CLOUDINIT = HERE / "vultr_cloudinit.sh"

VULTR_API = "https://api.vultr.com/v2"
HEALTH_URL = "https://api.flipster.io/api/v2/health"
COOLDOWN_S = 3600  # don't rotate same IP twice within 1h


@dataclass
class Proxy:
    url: str
    user: str
    password: str
    ip: str
    port: int
    instance_id: Optional[str] = None
    region: str = "icn"
    plan: str = "vc2-1c-1gb"
    os_id: int = 0  # filled at creation time

    @classmethod
    def from_url(cls, u: str) -> "Proxy":
        # http://user:pass@ip:port
        m = re.match(r"http://([^:]+):([^@]+)@([^:]+):(\d+)", u)
        if not m:
            raise ValueError(f"bad proxy url: {u}")
        return cls(
            url=u, user=m.group(1), password=m.group(2),
            ip=m.group(3), port=int(m.group(4)),
        )


# ----------------------------------------------------------------------
# .env parsing / writing
# ----------------------------------------------------------------------

def read_pool() -> list[Proxy]:
    txt = ENV_FILE.read_text()
    m = re.search(r"^FLIPSTER_PROXY_POOL=(.+)$", txt, re.M)
    if not m:
        return []
    return [Proxy.from_url(u.strip()) for u in m.group(1).split(",") if u.strip()]


def write_pool(pool: list[Proxy]) -> None:
    txt = ENV_FILE.read_text()
    new_line = "FLIPSTER_PROXY_POOL=" + ",".join(p.url for p in pool)
    new_txt = re.sub(r"^FLIPSTER_PROXY_POOL=.+$", new_line, txt, flags=re.M)
    if new_txt == txt:
        return
    ENV_FILE.write_text(new_txt)
    print(f"[env] FLIPSTER_PROXY_POOL updated ({len(pool)} entries)")


# ----------------------------------------------------------------------
# State persistence (instance metadata)
# ----------------------------------------------------------------------

def load_state() -> dict:
    if STATE_FILE.exists():
        return json.loads(STATE_FILE.read_text())
    return {"proxies": {}, "rotated_at": {}}


def save_state(state: dict) -> None:
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    STATE_FILE.write_text(json.dumps(state, indent=2))


# ----------------------------------------------------------------------
# Vultr API
# ----------------------------------------------------------------------

def vultr_api_key() -> str:
    k = os.environ.get("VULTR_API_KEY") or _read_env_var("VULTR_API_KEY")
    if not k:
        raise RuntimeError("VULTR_API_KEY missing — set it in .env or environment")
    return k


def _read_env_var(name: str) -> Optional[str]:
    if not ENV_FILE.exists():
        return None
    for line in ENV_FILE.read_text().splitlines():
        m = re.match(rf"^\s*{re.escape(name)}=(.*)$", line)
        if m:
            v = m.group(1).strip().strip('"').strip("'")
            return v
    return None


def vultr_request(method: str, path: str, body: Optional[dict] = None) -> dict:
    url = f"{VULTR_API}{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers={
        "Authorization": f"Bearer {vultr_api_key()}",
        "Content-Type": "application/json",
    })
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            txt = r.read().decode()
            return json.loads(txt) if txt else {}
    except urllib.error.HTTPError as e:
        body_txt = e.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"Vultr {method} {path} → {e.code}: {body_txt[:300]}") from e


def vultr_list_instances() -> list[dict]:
    r = vultr_request("GET", "/instances")
    return r.get("instances", [])


def vultr_destroy(instance_id: str) -> None:
    print(f"[vultr] destroying {instance_id[:18]}…")
    vultr_request("DELETE", f"/instances/{instance_id}")


def vultr_create(region: str, plan: str, os_id: int, label: str) -> dict:
    user_data_b64 = base64.b64encode(CLOUDINIT.read_bytes()).decode()
    body = {
        "region": region,
        "plan": plan,
        "os_id": os_id,
        "label": label,
        "user_data": user_data_b64,
        "enable_ipv6": False,
        "backups": "disabled",
        "ddos_protection": False,
    }
    r = vultr_request("POST", "/instances", body)
    inst = r["instance"]
    print(f"[vultr] created {inst['id'][:18]} (waiting for IP & cloud-init)")
    return inst


def wait_for_active_ip(instance_id: str, timeout_s: int = 240) -> str:
    """Poll Vultr API until the new instance has a non-empty main_ip."""
    t0 = time.time()
    while time.time() - t0 < timeout_s:
        r = vultr_request("GET", f"/instances/{instance_id}")
        inst = r.get("instance", {})
        ip = inst.get("main_ip", "")
        status = inst.get("status", "")
        server_status = inst.get("server_status", "")
        if ip and ip != "0.0.0.0" and status == "active" and server_status in ("ok", "installingbooting"):
            return ip
        time.sleep(5)
    raise TimeoutError(f"instance {instance_id} not active within {timeout_s}s")


# ----------------------------------------------------------------------
# Health check via proxy
# ----------------------------------------------------------------------

def proxy_health(p: Proxy, timeout: int = 8) -> tuple[int, str]:
    """Return (http_status, body). 200 = healthy, 429 = blocked."""
    handler = urllib.request.ProxyHandler({"http": p.url, "https": p.url})
    opener = urllib.request.build_opener(handler)
    req = urllib.request.Request(HEALTH_URL, headers={"User-Agent": "Mozilla/5.0"})
    try:
        with opener.open(req, timeout=timeout) as r:
            return r.status, r.read()[:80].decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:80].decode("utf-8", errors="replace")
    except Exception as e:
        return -1, str(e)[:80]


def wait_for_proxy_ready(p: Proxy, timeout_s: int = 300) -> bool:
    """Poll the new proxy until tinyproxy answers and Flipster /health
    returns 200 through it. cloud-init takes 90-180s on Vultr Seoul."""
    print(f"[wait] {p.ip}:3128 cloud-init bootup (up to {timeout_s}s)…")
    t0 = time.time()
    last_log = 0
    while time.time() - t0 < timeout_s:
        status, _ = proxy_health(p, timeout=6)
        elapsed = int(time.time() - t0)
        if status == 200:
            print(f"[wait] {p.ip} ready after {elapsed}s ✓")
            return True
        if elapsed - last_log > 20:
            print(f"[wait] {p.ip} {elapsed}s status={status}")
            last_log = elapsed
        time.sleep(5)
    print(f"[wait] {p.ip} TIMED OUT after {timeout_s}s — keeping anyway")
    return False


# ----------------------------------------------------------------------
# Rotation orchestrator
# ----------------------------------------------------------------------

def rotate(p: Proxy, state: dict) -> Proxy:
    """Destroy `p`, create a fresh instance with same region/plan/os,
    wait for proxy ready, return the replacement Proxy."""
    rotated_at = state.setdefault("rotated_at", {})
    now = int(time.time())
    last = rotated_at.get(p.ip, 0)
    if now - last < COOLDOWN_S:
        wait = COOLDOWN_S - (now - last)
        print(f"[rotate] {p.ip} on cooldown ({wait}s) — skipping")
        return p

    if not p.instance_id:
        # backfill from API by IP match
        for inst in vultr_list_instances():
            if inst.get("main_ip") == p.ip:
                p.instance_id = inst["id"]
                p.region = inst.get("region", p.region)
                p.plan = inst.get("plan", p.plan)
                p.os_id = inst.get("os_id", 0) or 0
                break

    if not p.instance_id:
        print(f"[rotate] {p.ip} not found in Vultr — leaving in pool")
        return p

    label = f"flipster-proxy-{int(time.time())}"
    region, plan, os_id = p.region, p.plan, p.os_id

    vultr_destroy(p.instance_id)
    new_inst = vultr_create(region=region, plan=plan, os_id=os_id, label=label)
    new_ip = wait_for_active_ip(new_inst["id"])

    new_proxy = Proxy(
        url=f"http://{p.user}:{p.password}@{new_ip}:{p.port}",
        user=p.user, password=p.password, ip=new_ip, port=p.port,
        instance_id=new_inst["id"], region=region, plan=plan, os_id=os_id,
    )
    wait_for_proxy_ready(new_proxy)

    rotated_at[new_ip] = int(time.time())
    if p.ip in rotated_at:
        del rotated_at[p.ip]
    proxies = state.setdefault("proxies", {})
    proxies[new_ip] = {
        "id": new_inst["id"], "region": region, "plan": plan, "os_id": os_id,
        "created_at": int(time.time()),
    }
    proxies.pop(p.ip, None)
    save_state(state)
    print(f"[rotate] {p.ip} → {new_ip} ✓")
    return new_proxy


def restart_executor() -> None:
    print("[exec] bouncing collector + executor to reload pool")
    subprocess.run([str(PROJECT / "scripts" / "control.sh"), "gl-stop"], check=False)
    subprocess.run([str(PROJECT / "scripts" / "control.sh"), "collector-stop"], check=False)
    time.sleep(2)
    subprocess.run([str(PROJECT / "scripts" / "control.sh"), "collector-start"], check=False)
    subprocess.run([str(PROJECT / "scripts" / "control.sh"), "gl-start"], check=False)


def scan_and_rotate(pool: list[Proxy], state: dict) -> tuple[list[Proxy], int]:
    """Health-check the whole pool. Returns (new_pool, rotations_done)."""
    rotations = 0
    out = []
    for p in pool:
        status, body = proxy_health(p)
        if status == 429:
            print(f"[health] {p.ip} BLOCKED (429) — rotating")
            new_p = rotate(p, state)
            out.append(new_p)
            if new_p.ip != p.ip:
                rotations += 1
        else:
            print(f"[health] {p.ip} status={status} ✓")
            # backfill instance_id if missing
            if not p.instance_id:
                proxies = state.setdefault("proxies", {})
                if p.ip in proxies:
                    p.instance_id = proxies[p.ip].get("id")
            out.append(p)
    return out, rotations


def cmd_status() -> None:
    pool = read_pool()
    inventory = vultr_list_instances()
    inv_by_ip = {i.get("main_ip"): i for i in inventory}
    print(f"=== Pool from .env ({len(pool)}) ===")
    for p in pool:
        inv = inv_by_ip.get(p.ip)
        if inv:
            print(f"  {p.ip:18s} status={inv.get('status'):8s} region={inv.get('region')} plan={inv.get('plan')}")
        else:
            print(f"  {p.ip:18s} ⚠️  not in Vultr inventory")
    print()
    print("=== Health ===")
    for p in pool:
        s, _ = proxy_health(p)
        marker = "✓" if s == 200 else ("🚫" if s == 429 else "?")
        print(f"  {p.ip:18s} status={s} {marker}")


def cmd_once(rotate_all: bool = False) -> None:
    pool = read_pool()
    state = load_state()
    if rotate_all:
        # Parallel: rotate all 5 at once. Cuts wall time from ~15min to
        # ~3-5min (dominated by Vultr provision + cloud-init).
        with ThreadPoolExecutor(max_workers=len(pool)) as ex:
            new_pool = list(ex.map(lambda p: rotate(p, state), pool))
        rotations = sum(1 for old, new in zip(pool, new_pool) if old.ip != new.ip)
    else:
        new_pool, rotations = scan_and_rotate(pool, state)
    if rotations > 0:
        write_pool(new_pool)
        save_state(state)
        restart_executor()
    else:
        print("[once] no rotations needed")


def cmd_watch(interval_s: int = 30) -> None:
    print(f"[watch] daemon mode, scan every {interval_s}s. Ctrl-C to stop.")
    while True:
        try:
            cmd_once()
        except Exception as e:
            print(f"[watch] iter err: {e}")
        time.sleep(interval_s)


# ----------------------------------------------------------------------

def main() -> None:
    p = argparse.ArgumentParser()
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--once", action="store_true", help="scan + rotate blocked, exit")
    g.add_argument("--watch", action="store_true", help="daemon mode, repeat every 30s")
    g.add_argument("--status", action="store_true", help="print health + inventory")
    g.add_argument("--rotate-all", action="store_true", help="force rotate every proxy")
    args = p.parse_args()

    if args.status:
        cmd_status()
    elif args.once:
        cmd_once()
    elif args.rotate_all:
        cmd_once(rotate_all=True)
    elif args.watch:
        cmd_watch()


if __name__ == "__main__":
    main()
