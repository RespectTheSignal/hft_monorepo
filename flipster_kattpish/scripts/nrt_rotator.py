#!/usr/bin/env python3
"""Auto-rotate the NRT executor instance when Cloudflare rate-limits its IP.

Different from proxy_rotator.py: that one rotates a *pool* of HTTP-CONNECT
proxies on gate1. This one rotates the *single* Tokyo executor instance
that runs collector + executor + maintains the trading session.

Detection:
  - SSH into NRT, grep last 60s of executor log for "status 429" or
    1015 / "rate-limited" markers.
  - Or curl https://api.flipster.io/api/v2/health from NRT and check status.

Rotation (blue-green):
  1. Spin up a NEW NRT instance from the saved snapshot (~2-3 min).
  2. SSH in, sync latest cookies (rsync from gate1).
  3. Start collector + executor (control.sh).
  4. Verify: curl health = 200, executor logs show entries.
  5. Update gate1 cron to push cookies to the NEW IP.
  6. Destroy old NRT.

Cooldown: 1h between rotations. Cleans up dangling snapshots if the
script crashes mid-rotation (re-runnable).

Usage:
  nrt_rotator.py --status     # health snapshot, IP, cooldown
  nrt_rotator.py --once       # detect 429, rotate if needed, exit
  nrt_rotator.py --watch      # daemon, scan every 60s
  nrt_rotator.py --rotate     # force rotation now
  nrt_rotator.py --snapshot   # take a fresh snapshot from current NRT
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
from pathlib import Path
from typing import Optional

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parent
STATE_FILE = PROJECT / "data" / "nrt_state.json"
LIFECYCLE_LOG = PROJECT / "logs" / "nrt_lifecycle.jsonl"
VULTR_API = "https://api.vultr.com/v2"
# Short cooldown after rotation purely to prevent buggy-detection
# loops (e.g. half-completed swap). Hot-standby setup means the
# actual swap is ~10s so a 60s cooldown is plenty.
COOLDOWN_S = 60
COOKIE_PATH = "/home/gate1/.config/flipster_kattpish/cookies.json"
NRT_REMOTE_COOKIE = "/root/.config/flipster_kattpish/cookies.json"


def lifecycle_event(event: str, **fields) -> None:
    """Append a JSONL row to logs/nrt_lifecycle.jsonl. Auto-fetches
    Flipster USDT marginBalance via WS so each row carries the start/
    end balance for that IP's lifecycle. Subtracting consecutive
    margin_balance values gives the per-IP PnL contribution.
    event ∈ {"started", "blocked"}."""
    LIFECYCLE_LOG.parent.mkdir(parents=True, exist_ok=True)
    bal = fetch_margin_balance()
    row = {"ts": int(time.time()), "iso": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
           "event": event, "margin_balance": bal, **fields}
    with LIFECYCLE_LOG.open("a") as f:
        f.write(json.dumps(row) + "\n")


def fetch_margin_balance() -> Optional[float]:
    """SSH to current NRT and grab Flipster USDT marginBalance from
    the private/margins WS snapshot. Runs from NRT because gate1's
    direct WS handshake gets CF-rejected (403) — gate1 has no proxy
    for WS, NRT has direct Tokyo→Flipster access. Returns None if NRT
    unreachable or WS times out."""
    state = load_state()
    nrt_ip = state.get("ip")
    if not nrt_ip:
        return None
    script = '''
import asyncio, json, sys, time
try:
    import websockets
except Exception as e:
    print("err: websockets not installed:", e); sys.exit(1)
ck = json.load(open("/root/.config/flipster_kattpish/cookies.json"))["flipster"]
ck_str = "; ".join(f"{k}={v}" for k,v in ck.items())
async def _grab():
    url = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
    h = {"Origin":"https://flipster.io","Cookie":ck_str,"User-Agent":"Mozilla/5.0"}
    async with websockets.connect(url, additional_headers=h, ping_interval=20, open_timeout=10) as ws:
        await ws.send(json.dumps({"s": {"private/margins": {"rows": ["*"]}}}))
        t0 = time.time()
        while time.time() - t0 < 5:
            try: msg = await asyncio.wait_for(ws.recv(), timeout=2)
            except asyncio.TimeoutError: continue
            d = json.loads(msg)
            usdt = d.get("t",{}).get("private/margins",{}).get("s",{}).get("USDT",{})
            if "marginBalance" in usdt:
                print(usdt["marginBalance"]); return
asyncio.run(_grab())
'''
    rc, out = ssh(nrt_ip, f"python3 -c '{script}'", timeout=15)
    if rc != 0:
        print(f"[balance] ssh fail rc={rc}: {out[:200]}")
        return None
    out = out.strip()
    try:
        return float(out.split("\n")[-1])
    except Exception:
        print(f"[balance] parse err: {out[:200]}")
        return None


# ----------------------------------------------------------------------
# State
# ----------------------------------------------------------------------

def load_state() -> dict:
    if STATE_FILE.exists():
        return json.loads(STATE_FILE.read_text())
    return {}


def save_state(s: dict) -> None:
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    STATE_FILE.write_text(json.dumps(s, indent=2))


# ----------------------------------------------------------------------
# Vultr API
# ----------------------------------------------------------------------

def vultr_api_key() -> str:
    k = os.environ.get("VULTR_API_KEY")
    if k:
        return k
    env = PROJECT / ".env"
    if env.exists():
        for line in env.read_text().splitlines():
            m = re.match(r"^\s*VULTR_API_KEY=(.*)$", line)
            if m:
                return m.group(1).strip().strip('"').strip("'")
    raise RuntimeError("VULTR_API_KEY not in env or .env")


def vultr(method: str, path: str, body: Optional[dict] = None) -> dict:
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(f"{VULTR_API}{path}", data=data, method=method,
        headers={"Authorization": f"Bearer {vultr_api_key()}",
                 "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            txt = r.read().decode()
            return json.loads(txt) if txt else {}
    except urllib.error.HTTPError as e:
        body_txt = e.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"Vultr {method} {path} → {e.code}: {body_txt[:200]}") from e


def vultr_create_from_snapshot(snapshot_id: str, label: str,
                                region: str, plan: str) -> dict:
    body = {
        "region": region, "plan": plan,
        "snapshot_id": snapshot_id, "label": label,
        "enable_ipv6": False, "backups": "disabled",
    }
    r = vultr("POST", "/instances", body)
    return r["instance"]


def vultr_destroy(instance_id: str) -> None:
    vultr("DELETE", f"/instances/{instance_id}")


def vultr_get(instance_id: str) -> dict:
    return vultr("GET", f"/instances/{instance_id}")["instance"]


def wait_active_ip(instance_id: str, timeout_s: int = 240) -> str:
    t0 = time.time()
    while time.time() - t0 < timeout_s:
        inst = vultr_get(instance_id)
        if inst["main_ip"] and inst["main_ip"] != "0.0.0.0" and inst["status"] == "active":
            return inst["main_ip"]
        time.sleep(5)
    raise TimeoutError(f"instance {instance_id} not active in {timeout_s}s")


# ----------------------------------------------------------------------
# SSH helpers
# ----------------------------------------------------------------------

def ssh(ip: str, cmd: str, timeout: int = 30) -> tuple[int, str]:
    """Run command on NRT via ssh. Returns (rc, output). Returns (124, "")
    on subprocess timeout — kept non-fatal so ssh_ready's poll loop can
    keep retrying instead of bubbling an exception up to rotate(), which
    used to spawn another instance on every 60s tick (runaway orphans)."""
    try:
        p = subprocess.run(
            ["ssh", "-o", "StrictHostKeyChecking=no",
             "-o", "ConnectTimeout=10",
             "-o", "BatchMode=yes",
             f"root@{ip}", cmd],
            capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return 124, ""
    except Exception as e:
        return 1, f"ssh err: {e}"
    return p.returncode, p.stdout + p.stderr


def ssh_ready(ip: str, timeout_s: int = 180) -> bool:
    t0 = time.time()
    while time.time() - t0 < timeout_s:
        rc, _ = ssh(ip, "echo ready", timeout=8)
        if rc == 0:
            return True
        time.sleep(5)
    return False


# ----------------------------------------------------------------------
# Health check
# ----------------------------------------------------------------------

def is_blocked(ip: str) -> bool:
    """Returns True if NRT shows signs of CF rate limiting on any
    endpoint we actually use. Two-stage check:
      1. Tail executor log for trade-endpoint failures in last 90s.
         CF 1015 on /trade/* hangs for ~30s before erroring, so the
         trail of 'rtt_ms=30000+' entries is the real signal — /health
         keeps returning 200 even when /trade is dead.
      2. Fallback to /health curl in case executor log is stale or the
         block hits before the executor sees a signal."""
    # Stage 1: executor log analysis
    rc, out = ssh(ip,
        "tail -300 /root/projects/quant/hft_monorepo/flipster_kattpish/logs/executor_v44.log "
        "| awk -v cutoff=$(date -u -d '90 seconds ago' +%s) '"
        "{ match($0, /T([0-9]+):([0-9]+):([0-9]+)/, m); "
        "  if (m[0]) print $0 }' "
        "| grep -cE 'MARKET-ENTRY] FAILED|FLIPSTER-LIMIT] FAILED|status 429|rtt_ms=3[0-9]{4}'",
        timeout=15)
    if rc == 0:
        try:
            n_fails = int(out.strip().split('\n')[-1])
            if n_fails >= 5:
                print(f"[health] {ip} BLOCKED: {n_fails} trade failures in tail")
                return True
        except Exception:
            pass

    # Stage 2: curl /health
    rc, out = ssh(ip,
        "curl -s -o /dev/null -w '%{http_code}' --max-time 5 https://api.flipster.io/api/v2/health",
        timeout=15)
    if rc != 0:
        return False
    code = out.strip()
    if code == "429":
        print(f"[health] {ip} BLOCKED: /health returned 429")
        return True
    return False


# ----------------------------------------------------------------------
# Rotation
# ----------------------------------------------------------------------

def rotate(state: dict) -> dict:
    """Hot-standby swap: standby is already booted with services
    running, so blocked → swap is ~5 sec instead of ~3 min. After
    swap, provision the next standby in background so the next swap
    is also instant. Falls back to cold-create if no standby is
    ready (first run, or standby died)."""
    snapshot_id = state["snapshot_id"]
    region = state.get("region", "nrt")
    plan = state.get("plan", "vhf-2c-4gb")
    old_id = state["id"]
    old_ip = state["ip"]

    standby_id = state.get("standby_id")
    standby_ip = state.get("standby_ip")
    new_id = None
    new_ip = None

    if standby_id and standby_ip:
        # Verify: Flipster reachable + collector running (executor is
        # parked on standbys, only started on promotion below)
        rc, out = ssh(standby_ip,
            "curl -s -o /dev/null -w '%{http_code}' --max-time 5 https://api.flipster.io/api/v2/health "
            "&& echo procs:$(ps -ef | grep 'target/release/collector' | grep -v grep | wc -l)",
            timeout=15)
        is_healthy = rc == 0 and "200" in out and "procs:1" in out
        if is_healthy:
            new_id, new_ip = standby_id, standby_ip
            print(f"[rotate] HOT-STANDBY ready: {new_ip}")
        else:
            print(f"[rotate] standby {standby_ip} unhealthy ({out[:80]}); cold-create fallback")
            try:
                vultr_destroy(standby_id)
            except Exception:
                pass
            standby_id = standby_ip = None

    if not new_id:
        # Cold path: create from snapshot, wait for boot
        label = f"flipster-trading-{int(time.time())}"
        print(f"[rotate] cold-creating new NRT from snapshot…")
        new_inst = vultr_create_from_snapshot(snapshot_id, label, region, plan)
        new_id = new_inst["id"]
        new_ip = wait_active_ip(new_id)
        print(f"[rotate] new instance {new_id[:18]} ip={new_ip}")
        if not ssh_ready(new_ip, timeout_s=300):
            print(f"[rotate] SSH not ready on {new_ip}; abort + cleanup")
            try: vultr_destroy(new_id)
            except Exception: pass
            raise RuntimeError("ssh timeout on new instance")

    # Sync fresh cookies BEFORE starting executor (snapshot's cookies are stale)
    rc = subprocess.run(
        ["rsync", "-a", COOKIE_PATH, f"root@{new_ip}:{NRT_REMOTE_COOKIE}"],
        capture_output=True, text=True, timeout=60,
    ).returncode
    if rc != 0:
        print(f"[rotate] rsync cookies failed; continuing anyway")
    else:
        print(f"[rotate] cookies synced")

    # Start services. Standby already has collector running, so we
    # only need to start the executor on promotion. Cold-path
    # instances need both. control.sh's "already running" guard
    # makes the call idempotent in either case.
    rc, out = ssh(new_ip,
        "cd /root/projects/quant/hft_monorepo/flipster_kattpish && "
        "./scripts/control.sh collector-start && sleep 2 && "
        "./scripts/control.sh gl-start", timeout=30)
    print(f"[rotate] start: rc={rc}\n{out[:400]}")

    # Verify executor is alive
    time.sleep(8)
    rc, out = ssh(new_ip,
        "ps -ef | grep -E 'target/release/(collector|executor)' | grep -v grep | wc -l",
        timeout=10)
    procs = int(out.strip().split('\n')[0]) if out.strip() else 0
    if procs < 2:
        print(f"[rotate] only {procs} procs running on new NRT; abort + cleanup")
        vultr_destroy(new_id)
        raise RuntimeError("services failed to start on new instance")

    # Update gate1 cron with new IP
    update_cookie_cron(new_ip)

    # Destroy old
    print(f"[rotate] destroying old NRT {old_id[:18]} ({old_ip})…")
    try:
        vultr_destroy(old_id)
    except Exception as e:
        print(f"[rotate] old destroy err (ignore): {e}")

    state["id"] = new_id
    state["ip"] = new_ip
    state["last_rotated_at"] = int(time.time())
    state["started_at"] = int(time.time())
    state["standby_id"] = None
    state["standby_ip"] = None
    save_state(state)
    lifecycle_event("started", ip=new_ip, id=new_id, snapshot_id=snapshot_id,
                    replaced=old_ip)
    print(f"[rotate] done: {old_ip} → {new_ip}")

    # Provision next standby in background (don't block this rotation)
    try:
        provision_standby(state)
    except Exception as e:
        print(f"[rotate] standby provision failed (will retry next tick): {e}")

    return state


def provision_standby(state: dict) -> None:
    """Spin up a fresh instance from snapshot, wait until services are
    running, then save its id/ip in state.standby_*. Next rotate()
    promotes it instantly. Idempotent: skips if a healthy standby
    already exists."""
    if state.get("standby_ip"):
        rc, out = ssh(state["standby_ip"],
            "curl -s -o /dev/null -w '%{http_code}' --max-time 5 https://api.flipster.io/api/v2/health "
            "&& echo procs:$(ps -ef | grep -E 'target/release/(collector|executor) ' | grep -v grep | wc -l)",
            timeout=15)
        if rc == 0 and "200" in out and "procs:2" in out:
            print(f"[standby] {state['standby_ip']} still healthy; skip")
            return
        # else fall through to recreate

    label = f"flipster-standby-{int(time.time())}"
    print(f"[standby] provisioning new standby from snapshot…")
    inst = vultr_create_from_snapshot(state["snapshot_id"], label,
                                       state.get("region","nrt"),
                                       state.get("plan","vhf-2c-4gb"))
    sid = inst["id"]
    sip = wait_active_ip(sid)
    print(f"[standby] booting {sid[:18]} ip={sip}")

    if not ssh_ready(sip, timeout_s=300):
        print(f"[standby] {sip} SSH timeout; cleanup")
        try: vultr_destroy(sid)
        except Exception: pass
        return

    # Wire it up: cookies, env, services, websockets
    rc1 = subprocess.run(["rsync", "-a", COOKIE_PATH,
                          f"root@{sip}:{NRT_REMOTE_COOKIE}"],
                         capture_output=True, timeout=60).returncode
    rc2 = subprocess.run(["rsync", "-a", str(PROJECT/".env"),
                          f"root@{sip}:/root/projects/quant/hft_monorepo/flipster_kattpish/.env"],
                         capture_output=True, timeout=60).returncode
    ssh(sip, "pip install --break-system-packages -q websockets 2>&1 | tail -1", timeout=60)
    # Standby gets collector ONLY (so depth WS is warm + signals are
    # ZMQ'd in). The executor is NOT started — otherwise both A and B
    # would place orders against the same Flipster account, causing
    # double-fills and reconciliation hell. On promotion the rotate()
    # path re-runs control.sh gl-start to bring up the executor.
    rc, out = ssh(sip,
        "cd /root/projects/quant/hft_monorepo/flipster_kattpish && "
        "./scripts/control.sh collector-start", timeout=30)
    print(f"[standby] collector started rc={rc}")

    time.sleep(5)
    rc, out = ssh(sip,
        "ps -ef | grep 'target/release/collector' | grep -v grep | wc -l",
        timeout=10)
    procs = int(out.strip().split('\n')[0]) if out.strip() else 0
    if procs < 1:
        print(f"[standby] {sip} collector failed to start; cleanup")
        try: vultr_destroy(sid)
        except Exception: pass
        return

    # Health check: Flipster API reachable from this IP?
    rc, out = ssh(sip,
        "curl -s -o /dev/null -w '%{http_code}' --max-time 5 https://api.flipster.io/api/v2/health",
        timeout=15)
    if "200" not in out:
        print(f"[standby] {sip} health=={out!r}; CF-blocked already? cleanup")
        try: vultr_destroy(sid)
        except Exception: pass
        return

    state["standby_id"] = sid
    state["standby_ip"] = sip
    save_state(state)
    lifecycle_event("standby_ready", ip=sip, id=sid)
    print(f"[standby] HOT READY: {sip} (collector running, executor parked)")


def update_cookie_cron(new_ip: str) -> None:
    """Update gate1 cron line that pushes cookies to NRT to use new IP."""
    cur = subprocess.run(["crontab", "-l"], capture_output=True, text=True)
    lines = cur.stdout.splitlines() if cur.returncode == 0 else []
    new_lines = [l for l in lines if "flipster_kattpish/cookies.json" not in l]
    new_lines.append(
        f"*/5 * * * * rsync -a {COOKIE_PATH} root@{new_ip}:{NRT_REMOTE_COOKIE} >/dev/null 2>&1"
    )
    p = subprocess.run(["crontab", "-"], input="\n".join(new_lines) + "\n",
                       text=True, capture_output=True)
    if p.returncode == 0:
        print(f"[cron] cookie sync target → {new_ip}")
    else:
        print(f"[cron] update failed: {p.stderr}")


# ----------------------------------------------------------------------
# Snapshot
# ----------------------------------------------------------------------

def cmd_snapshot() -> None:
    state = load_state()
    if not state.get("id"):
        raise RuntimeError("no state.id — run --status first to seed")
    body = {"instance_id": state["id"], "description": "flipster-trading-base"}
    r = vultr("POST", "/snapshots", body)
    snap = r["snapshot"]
    print(f"[snapshot] created {snap['id']} status={snap['status']}")
    state["snapshot_id"] = snap["id"]
    save_state(state)


def wait_snapshot_ready(snap_id: str, timeout_s: int = 1800) -> bool:
    t0 = time.time()
    while time.time() - t0 < timeout_s:
        snaps = vultr("GET", "/snapshots").get("snapshots", [])
        match = [s for s in snaps if s["id"] == snap_id]
        if match and match[0]["status"] == "complete":
            return True
        time.sleep(15)
    return False


# ----------------------------------------------------------------------
# Commands
# ----------------------------------------------------------------------

def cmd_status() -> None:
    state = load_state()
    print(f"=== nrt state ===")
    print(json.dumps(state, indent=2))
    if state.get("ip"):
        rc, out = ssh(state["ip"],
            "curl -s -o /dev/null -w '%{http_code}' --max-time 5 https://api.flipster.io/api/v2/health",
            timeout=15)
        print(f"\nflipster /health from {state['ip']}: {out.strip() if rc==0 else 'SSH FAIL'}")
    if state.get("snapshot_id"):
        snaps = vultr("GET", "/snapshots").get("snapshots", [])
        for s in snaps:
            if s["id"] == state["snapshot_id"]:
                print(f"\nsnapshot: {s['id']} status={s['status']} size={s['size']}")
                break


def cmd_once() -> None:
    state = load_state()
    if not state.get("ip"):
        print("[once] no nrt state — nothing to monitor")
        return
    if not state.get("snapshot_id"):
        print("[once] no snapshot — can't rotate. run --snapshot first")
        return
    last = state.get("last_rotated_at", 0)
    if time.time() - last < COOLDOWN_S:
        wait = COOLDOWN_S - (time.time() - last)
        print(f"[once] cooldown active ({int(wait)}s); skipping")
        return
    if is_blocked(state["ip"]):
        started_at = state.get("started_at", 0)
        lifetime_s = int(time.time()) - started_at if started_at else None
        lifecycle_event("blocked", ip=state["ip"], id=state.get("id"),
                        started_at=started_at, lifetime_s=lifetime_s)
        print(f"[once] {state['ip']} BLOCKED (lifetime={lifetime_s}s) — rotating")
        rotate(state)
    else:
        print(f"[once] {state['ip']} healthy")
        # Pre-provision standby if none — first run, or previous
        # standby got promoted/destroyed and rotate() didn't backfill.
        if not state.get("standby_ip"):
            try:
                provision_standby(state)
            except Exception as e:
                print(f"[once] standby provision err: {e}")


def cmd_rotate() -> None:
    state = load_state()
    if not state.get("snapshot_id"):
        print("[rotate] no snapshot — run --snapshot first")
        return
    rotate(state)


def cmd_watch(interval_s: int = 60) -> None:
    print(f"[watch] daemon, scan every {interval_s}s")
    while True:
        try:
            cmd_once()
        except Exception as e:
            print(f"[watch] err: {e}")
        time.sleep(interval_s)


# ----------------------------------------------------------------------

def main() -> None:
    p = argparse.ArgumentParser()
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--status", action="store_true")
    g.add_argument("--once", action="store_true")
    g.add_argument("--watch", action="store_true")
    g.add_argument("--rotate", action="store_true")
    g.add_argument("--snapshot", action="store_true")
    args = p.parse_args()

    if args.status:
        cmd_status()
    elif args.once:
        cmd_once()
    elif args.watch:
        cmd_watch()
    elif args.rotate:
        cmd_rotate()
    elif args.snapshot:
        cmd_snapshot()


if __name__ == "__main__":
    main()
