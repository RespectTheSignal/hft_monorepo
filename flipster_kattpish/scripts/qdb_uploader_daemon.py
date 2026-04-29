"""Tailing daemon: watches executor JSONL trade logs and pushes new rows
to QuestDB position_log via ILP every 5 seconds.

Maintains a per-file byte offset in ~/.config/qdb_uploader_offsets.json
so it picks up where it left off after restart.
"""
from __future__ import annotations
import json, os, time, urllib.request, glob, signal, sys
from pathlib import Path
from datetime import datetime, timezone

QDB = "http://211.181.122.102:9000"
LOG_DIR = Path("/home/gate1/projects/quant/hft_monorepo/flipster_kattpish/logs")
PATTERN = "binance_lead_live*.jsonl"
STATE_FILE = Path.home() / ".config/qdb_uploader_offsets.json"
INTERVAL_S = 5
ACCOUNT = "BINANCE_LEAD_v1"
STRATEGY = "binance_lead_live"

_running = True
def _stop(*_):
    global _running
    _running = False
signal.signal(signal.SIGTERM, _stop)
signal.signal(signal.SIGINT, _stop)


def ts_to_ns(iso: str) -> int:
    iso = iso.replace("Z", "+00:00")
    dt = datetime.fromisoformat(iso)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1e9)

def esc(s: str) -> str:
    return s.replace(" ", "\\ ").replace(",", "\\,").replace("=", "\\=")

def jsonl_to_ilp(r: dict) -> str:
    size = float(r["size_usd"])
    pnl_bp = (float(r.get("net_after_fees_usd", 0)) / size * 1e4) if size > 0 else 0.0
    sym = r["base"] + "_USDT"
    side = r["flipster_side"]
    er = r.get("exit_reason", "?")
    en = ts_to_ns(r["ts_entry"])
    ex = ts_to_ns(r["ts_close"])
    return (f"position_log,account_id={ACCOUNT},symbol={esc(sym)},side={esc(side)},"
            f"strategy={STRATEGY},exit_reason={esc(er)},mode=live "
            f"entry_price={float(r.get('flipster_entry', 0))},"
            f"exit_price={float(r.get('flipster_exit', 0))},"
            f"size={size},pnl_bp={pnl_bp},exit_time={ex}t {en}")

def load_offsets() -> dict:
    if STATE_FILE.exists():
        try:
            return json.loads(STATE_FILE.read_text())
        except Exception:
            return {}
    return {}

def save_offsets(d: dict) -> None:
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    tmp = STATE_FILE.with_suffix(".tmp")
    tmp.write_text(json.dumps(d, indent=2))
    tmp.replace(STATE_FILE)

def upload(lines: list[str]) -> None:
    if not lines:
        return
    body = "\n".join(lines).encode()
    req = urllib.request.Request(f"{QDB}/write", data=body, method="POST",
                                  headers={"Content-Type":"text/plain"})
    resp = urllib.request.urlopen(req, timeout=30)
    if resp.status >= 400:
        raise RuntimeError(f"ILP {resp.status}: {resp.read()[:200]}")

def tick(offsets: dict) -> int:
    """Scan all matching files, upload any new rows. Returns count uploaded."""
    new_lines: list[str] = []
    for path_str in sorted(glob.glob(str(LOG_DIR / PATTERN))):
        path = Path(path_str)
        try:
            sz = path.stat().st_size
        except OSError:
            continue
        prev = offsets.get(path.name, 0)
        if sz < prev:
            # File rotated/truncated — restart from 0
            prev = 0
        if sz <= prev:
            offsets[path.name] = sz
            continue
        with path.open("rb") as f:
            f.seek(prev)
            buf = f.read(sz - prev)
        consumed = 0
        for raw in buf.splitlines(keepends=True):
            consumed += len(raw)
            txt = raw.strip()
            if not txt:
                continue
            try:
                r = json.loads(txt)
                new_lines.append(jsonl_to_ilp(r))
            except Exception as e:
                print(f"  parse err {path.name}: {e}", flush=True)
        offsets[path.name] = prev + consumed
    if new_lines:
        try:
            upload(new_lines)
            print(f"[{datetime.utcnow().isoformat()}Z] uploaded {len(new_lines)}", flush=True)
        except Exception as e:
            print(f"  ILP err: {e}", flush=True)
            # Don't advance offsets on upload failure — retry next tick
            return 0
    return len(new_lines)

def main() -> None:
    offsets = load_offsets()
    print(f"start daemon, interval={INTERVAL_S}s, log_dir={LOG_DIR}, offsets loaded: {len(offsets)} files", flush=True)
    last_save = 0.0
    while _running:
        try:
            n = tick(offsets)
            now = time.time()
            if n > 0 or now - last_save > 60:
                save_offsets(offsets)
                last_save = now
        except Exception as e:
            print(f"  tick err: {e}", flush=True)
        # Sleep with quick interrupt check
        for _ in range(INTERVAL_S):
            if not _running: break
            time.sleep(1)
    save_offsets(offsets)
    print("stopped", flush=True)

if __name__ == "__main__":
    main()
