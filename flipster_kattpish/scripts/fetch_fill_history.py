"""Fetch historical fee/fill records from both exchanges.

Gate: pulls /apiw/v2/futures/usdt/account_book — every PnL/fee record per fill
      with actual USDT amounts. Goldmine for fee-model validation.

Flipster: tries /api/v2/trade/orders/history etc. Some endpoints hit CF 1015
          if used too often; we use the live_executor's session if running, or
          start a fresh browser-cookie session otherwise.

Outputs:
  logs/gate_account_book.jsonl     — one event per line (pnl, fee, fund, ...)
  logs/flipster_fill_history.jsonl — one fill per line (if endpoint works)

Usage:
  python3 scripts/fetch_fill_history.py [--days 7]
"""
from __future__ import annotations
import argparse, importlib.util, json, sys, time
from datetime import datetime, timezone, timedelta
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
_QUANT = ROOT.parent
MONOREPO = _QUANT / "hft_monorepo" if (_QUANT / "hft_monorepo" / "flipster_web").exists() else _QUANT


def _load_pkg(name, root):
    pkg_dir = root / "python"
    spec = importlib.util.spec_from_file_location(
        name, pkg_dir / "__init__.py", submodule_search_locations=[str(pkg_dir)])
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


flipster_pkg = _load_pkg("flipster_pkg", MONOREPO / "flipster_web")
gate_pkg = _load_pkg("gate_pkg", MONOREPO / "gate_web")
from gate_pkg.client import GateClient, API_BASE as GATE_BASE
from flipster_pkg.client import FlipsterClient, API_BASE as FLIP_BASE


def fetch_gate_account_book(session, days: int) -> list[dict]:
    """Paginate through Gate account_book back `days` from now."""
    cutoff_ts = int((datetime.now(timezone.utc) - timedelta(days=days)).timestamp())
    out = []
    # Gate paginates by `last_id` and `from`. Try a few pages with limit=100.
    last_id = None
    for page in range(50):  # max 50 pages * 100 = 5000 rows; plenty
        url = f"{GATE_BASE}/apiw/v2/futures/usdt/account_book?limit=100"
        if last_id:
            url += f"&last_id={last_id}"
        try:
            r = session.get(url, timeout=15)
            body = r.json()
        except Exception as e:
            print(f"  page={page} err: {e}")
            break
        if not isinstance(body, dict):
            break
        rows = body.get("data") or []
        if not rows:
            break
        keep = []
        for row in rows:
            t = row.get("time")
            if isinstance(t, (int, float)) and t < cutoff_ts:
                continue
            keep.append(row)
        out.extend(keep)
        # Heuristic: if any row in the page is older than cutoff, stop
        if any(isinstance(row.get("time"), (int, float)) and row["time"] < cutoff_ts for row in rows):
            break
        # Update last_id from last row
        last = rows[-1]
        last_id = last.get("id")
        if not last_id:
            break
        time.sleep(0.2)
    return out


def fetch_flipster_fills(session, days: int) -> list[dict] | None:
    """Try several Flipster fill/order-history endpoints. Returns None on CF 1015."""
    candidates = [
        f"{FLIP_BASE}/api/v2/trade/orders/history",
        f"{FLIP_BASE}/api/v2/trade/fills",
        f"{FLIP_BASE}/api/v2/trade/trades",
        f"{FLIP_BASE}/api/v1/trade/fills",
        f"{FLIP_BASE}/api/v1/trade/history",
    ]
    for url in candidates:
        try:
            r = session.get(url, timeout=10)
            ct = r.headers.get("content-type", "")
            if "json" not in ct:
                print(f"  {url} -> non-json ({r.status_code})")
                continue
            body = r.json()
            if isinstance(body, dict) and body.get("error_code") == 1015:
                print(f"  {url} -> CF 1015 rate-limited")
                continue
            if r.status_code != 200:
                print(f"  {url} -> {r.status_code}: {str(body)[:120]}")
                continue
            print(f"  {url} -> 200 keys={list(body.keys())[:6] if isinstance(body, dict) else type(body).__name__}")
            # Try to extract list of fills
            data = body
            if isinstance(body, dict):
                for k in ("fills", "trades", "orders", "data", "items", "results"):
                    if k in body and isinstance(body[k], list):
                        data = body[k]
                        break
            if isinstance(data, list):
                return data
            print(f"    body snippet: {json.dumps(body)[:300]}")
        except Exception as e:
            print(f"  {url} err: {e}")
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=7)
    ap.add_argument("--skip-flipster", action="store_true")
    args = ap.parse_args()

    out_dir = ROOT / "logs"
    out_dir.mkdir(parents=True, exist_ok=True)

    # ---- Gate ----
    print(f"\n=== Gate account_book (last {args.days} days) ===")
    gc = GateClient()
    gc.start_browser()
    gc.login_done()
    rows = fetch_gate_account_book(gc._session, args.days)
    print(f"  fetched {len(rows)} events")
    if rows:
        # Summarize by type
        from collections import Counter, defaultdict
        types = Counter(r.get("type") for r in rows)
        sums = defaultdict(float)
        for r in rows:
            try:
                v = float(str(r.get("change", "0")).replace("USDT", "").strip().split()[0])
            except Exception:
                v = 0
            sums[r.get("type", "?")] += v
        print(f"  by type: {dict(types)}")
        for t, s in sums.items():
            print(f"    {t:<10} sum={s:+.6f} USDT")
        path = out_dir / "gate_account_book.jsonl"
        with open(path, "w") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")
        print(f"  saved → {path}")

    # ---- Flipster ----
    if not args.skip_flipster:
        print(f"\n=== Flipster fill history ===")
        fc = FlipsterClient()
        fc.start_browser()
        fc.login_done()
        fills = fetch_flipster_fills(fc._session, args.days)
        if fills:
            path = out_dir / "flipster_fill_history.jsonl"
            with open(path, "w") as f:
                for fill in fills:
                    f.write(json.dumps(fill) + "\n")
            print(f"  saved {len(fills)} fills → {path}")
        else:
            print("  no fill history endpoint succeeded — try later or via WS private feed")

    print("\ndone.")


if __name__ == "__main__":
    main()
