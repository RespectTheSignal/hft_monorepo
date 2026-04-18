#!/usr/bin/env python3
"""Live executor sidecar — mirrors paper bot signals to real exchanges.

Polls QuestDB `trade_signal` table for entry/exit events from a chosen
variant, then places real orders on Flipster + Gate via browser-cookie
clients.

Usage:
    # 1. Start browsers & login (one-time)
    python3 scripts/live_executor.py --setup

    # 2. Run live (after login)
    python3 scripts/live_executor.py --variant T01_best --size-usd 10

Environment:
    QUESTDB_HTTP_URL  (default: http://211.181.122.102:9000)
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import signal
import sys
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

# Both flipster_web and gate_web have a `python` subpackage — direct sys.path
# imports would collide. Load each one under a unique top-level name.
MONOREPO = Path(__file__).resolve().parent.parent.parent


def _load_pkg(name: str, root: Path):
    """Load `<root>/python/__init__.py` as a top-level module named `name`."""
    pkg_dir = root / "python"
    spec = importlib.util.spec_from_file_location(
        name, pkg_dir / "__init__.py", submodule_search_locations=[str(pkg_dir)]
    )
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


# Load both as distinct top-level packages.
flipster_pkg = _load_pkg("flipster_pkg", MONOREPO / "flipster_web")
gate_pkg = _load_pkg("gate_pkg", MONOREPO / "gate_web")

QUESTDB_URL = "http://211.181.122.102:9000"
POLL_INTERVAL = 0.5  # seconds

# Gate contract specs (multiplier, min_size) keyed by contract name.
# Loaded once at startup from public API.
GATE_CONTRACTS: dict[str, dict] = {}


def load_gate_contracts():
    """Fetch contract multipliers from Gate.io public API."""
    global GATE_CONTRACTS
    print("[init] Loading Gate contract specs...")
    try:
        resp = urllib.request.urlopen(
            "https://api.gateio.ws/api/v4/futures/usdt/contracts", timeout=10
        )
        data = json.loads(resp.read())
        for c in data:
            GATE_CONTRACTS[c["name"]] = {
                "multiplier": float(c["quanto_multiplier"]),
                "order_size_min": int(c.get("order_size_min", 1)),
            }
        print(f"[init] Loaded {len(GATE_CONTRACTS)} Gate contracts")
    except Exception as e:
        print(f"[init] WARNING: contract load failed: {e}")


def gate_size_from_usd(contract: str, usd: float, price: float) -> int:
    """Convert USD notional to Gate contract count.

    notional_per_contract = price * multiplier
    contracts = round(usd / notional_per_contract)
    """
    spec = GATE_CONTRACTS.get(contract)
    if spec is None or price <= 0:
        return 1
    notional_per_contract = price * spec["multiplier"]
    if notional_per_contract <= 0:
        return spec["order_size_min"]
    contracts = round(usd / notional_per_contract)
    return max(contracts, spec["order_size_min"])


# ---------------------------------------------------------------------------
# QuestDB polling
# ---------------------------------------------------------------------------

def query_qdb(sql: str) -> list[list]:
    url = f"{QUESTDB_URL}/exec?query={urllib.parse.quote(sql)}"
    resp = urllib.request.urlopen(url, timeout=5)
    data = json.loads(resp.read())
    return data.get("dataset", [])


# ---------------------------------------------------------------------------
# Position tracker
# ---------------------------------------------------------------------------

@dataclass
class LivePosition:
    position_id: int
    base: str
    flipster_side: str  # "long" or "short"
    size_usd: float
    entry_time: str
    # Real fill prices (filled in after orders complete)
    flipster_entry_price: float = 0.0
    flipster_size: float = 0.0  # in base coin units
    flipster_slot: Optional[int] = None
    gate_entry_price: float = 0.0
    gate_size: int = 0  # in contracts
    gate_contract: str = ""
    flipster_order: Optional[dict] = None
    gate_order: Optional[dict] = None


def _extract_flipster_fill(resp: dict) -> tuple[float, float, Optional[int]]:
    """Returns (avg_price, size, slot) from a Flipster open/close response."""
    pos = resp.get("position", {}) if isinstance(resp, dict) else {}
    order = resp.get("order", {}) if isinstance(resp, dict) else {}
    avg = float(pos.get("avgPrice") or order.get("avgPrice") or 0)
    size = abs(float(pos.get("size") or order.get("size") or 0))
    slot = pos.get("slot")
    if slot is None:
        slot = order.get("slot")
    return avg, size, slot


def _extract_gate_fill(resp: dict) -> tuple[float, int]:
    """Returns (fill_price, abs_size) from a Gate order response."""
    if not isinstance(resp, dict):
        return 0.0, 0
    inner = resp.get("data", {})
    if isinstance(inner, dict):
        d = inner.get("data", {})
        if isinstance(d, dict):
            fp = float(d.get("fill_price") or 0)
            sz = abs(int(d.get("size") or 0))
            return fp, sz
    return 0.0, 0


def _append_trade(path: Path, record: dict):
    """Append a closed-trade record to a JSONL file."""
    with open(path, "a") as f:
        f.write(json.dumps(record) + "\n")


@dataclass
class Executor:
    variant: str
    size_usd: float  # override paper size with this
    dry_run: bool = False
    symbol_whitelist: set[str] = field(default_factory=set)  # empty = all
    trade_log_path: Path = field(default_factory=lambda: Path("logs/live_trades.jsonl"))

    flipster_client: object = field(default=None, repr=False)
    gate_client: object = field(default=None, repr=False)

    open_positions: dict[int, LivePosition] = field(default_factory=dict)
    seen_entry_ids: set[int] = field(default_factory=set)
    seen_exit_ids: set[int] = field(default_factory=set)
    last_poll_ts: str = ""
    total_pnl_usd: float = 0.0
    trade_count: int = 0
    win_count: int = 0
    gate_leverage_set: set[str] = field(default_factory=set)
    gate_target_leverage: int = 10  # cross mode cap

    def poll_signals(self):
        """Poll for new entry/exit signals from the chosen variant."""
        # Only look at signals from the last 5 minutes to avoid replaying old ones
        sql = f"""
            SELECT position_id, base, action, flipster_side,
                   size_usd, flipster_price, gate_price, timestamp
            FROM trade_signal
            WHERE account_id = '{self.variant}'
              AND timestamp > dateadd('m', -5, now())
            ORDER BY timestamp ASC
        """
        try:
            rows = query_qdb(sql)
        except Exception as e:
            print(f"[poll] QuestDB error: {e}")
            return

        for row in rows:
            pos_id = int(row[0])
            base = row[1]
            action = row[2]
            side = row[3]
            size_usd = float(row[4])
            f_price = float(row[5])
            g_price = float(row[6])
            ts = row[7]

            if action == "entry" and pos_id not in self.seen_entry_ids:
                self.seen_entry_ids.add(pos_id)
                if self.symbol_whitelist and base not in self.symbol_whitelist:
                    continue  # skip — not in whitelist
                self._on_entry(pos_id, base, side, size_usd, f_price, g_price, ts)

            elif action == "exit" and pos_id not in self.seen_exit_ids:
                self.seen_exit_ids.add(pos_id)
                self._on_exit(pos_id, base, side, f_price, g_price, ts)

    def _on_entry(self, pos_id, base, side, paper_size, f_price, g_price, ts):
        flipster_sym = f"{base}USDT.PERP"
        gate_sym = f"{base}_USDT"
        size = self.size_usd

        print(f"\n{'='*60}")
        print(f"[ENTRY] {base} | side={side} | ${size:.0f}")
        print(f"  paper: flipster={f_price:.2f} gate={g_price:.2f}")
        print(f"  variant={self.variant} pos_id={pos_id}")

        if self.dry_run:
            print("  [DRY RUN] skipping real orders")
            self.open_positions[pos_id] = LivePosition(
                position_id=pos_id, base=base, flipster_side=side,
                size_usd=size, entry_time=ts,
            )
            return

        # Place Flipster FIRST. If it fails, skip Gate entirely (no unhedged risk).
        f_result = None
        try:
            from flipster_pkg.order import OrderParams as FParams, Side as FSide
            f_side = FSide.LONG if side == "long" else FSide.SHORT
            f_result = self.flipster_client.place_order(
                flipster_sym,
                FParams(side=f_side, amount_usd=size, leverage=10),
                ref_price=f_price,
            )
            print(f"  [FLIPSTER] OK: {json.dumps(f_result, default=str)[:200]}")
        except Exception as e:
            print(f"  [FLIPSTER] FAILED: {e} — SKIPPING Gate (avoid unhedged)")
            return

        # Place Gate hedge order. If this fails, IMMEDIATELY undo the Flipster side.
        from gate_pkg.order import OrderParams as GParams, Side as GSide
        g_side = GSide.SHORT if side == "long" else GSide.LONG
        g_size = gate_size_from_usd(gate_sym, size, g_price)
        # Set leverage on first encounter of each contract (CROSS mode, 10x cap)
        if gate_sym not in self.gate_leverage_set:
            try:
                lr = self.gate_client.set_leverage(
                    gate_sym, leverage=0, cross_leverage_limit=self.gate_target_leverage,
                )
                print(f"  [LEV] {gate_sym} → cross {self.gate_target_leverage}x: {lr.get('message', lr)[:80] if isinstance(lr, dict) else lr}")
                self.gate_leverage_set.add(gate_sym)
            except Exception as e:
                print(f"  [LEV] set_leverage failed: {e}")

        g_result = None
        try:
            g_result = self.gate_client.place_order(
                gate_sym,
                GParams(side=g_side, size=g_size),
            )
            ok = g_result.get("ok") if isinstance(g_result, dict) else False
            print(f"  [GATE] ok={ok} size={g_size}")
            if not ok:
                raise RuntimeError(f"gate not ok: {g_result}")
        except Exception as e:
            print(f"  [GATE] FAILED: {e}")
            print(f"  [SAFETY] Undoing Flipster side to stay flat...")
            slot = f_result.get("position", {}).get("slot")
            if slot is not None:
                undone = False
                offsets = [0.0, -0.005, -0.02] if side == "long" else [0.0, 0.005, 0.02]
                for off in offsets:
                    try:
                        try_px = f_price * (1 + off)
                        self.flipster_client.close_position(flipster_sym, slot, price=try_px)
                        print(f"  [SAFETY] Flipster reverted @ {try_px:.6f}")
                        undone = True
                        break
                    except Exception as e2:
                        if "InsufficientLiquidity" not in str(e2):
                            print(f"  [SAFETY] revert err: {e2}")
                            break
                if not undone:
                    print(f"  [SAFETY] !!! MANUAL INTERVENTION NEEDED — flipster slot={slot} still open !!!")
            return

        # Capture actual fill prices for PnL computation later.
        f_avg, f_size, f_slot = _extract_flipster_fill(f_result or {})
        g_avg, g_filled = _extract_gate_fill(g_result or {})
        lp = LivePosition(
            position_id=pos_id, base=base, flipster_side=side,
            size_usd=size, entry_time=ts,
            flipster_entry_price=f_avg,
            flipster_size=f_size,
            flipster_slot=f_slot,
            gate_entry_price=g_avg,
            gate_size=g_filled or g_size,
            gate_contract=gate_sym,
            flipster_order=f_result, gate_order=g_result,
        )
        self.open_positions[pos_id] = lp
        print(f"  [TRACK] flipster_entry={f_avg} size={f_size} slot={f_slot} | "
              f"gate_entry={g_avg} contracts={lp.gate_size}")

    def _on_exit(self, pos_id, base, side, f_price, g_price, ts):
        pos = self.open_positions.pop(pos_id, None)

        print(f"\n{'='*60}")
        print(f"[EXIT] {base} | side={side} | pos_id={pos_id}")
        print(f"  paper: flipster={f_price:.2f} gate={g_price:.2f}")

        if pos is None:
            print("  (position not tracked — skipped or from before startup)")
            return

        if self.dry_run:
            print("  [DRY RUN] skipping real close")
            self.trade_count += 1
            return

        flipster_sym = f"{base}USDT.PERP"
        gate_sym = f"{base}_USDT"

        # Close Flipster — try a few ref prices around f_price to handle
        # market drift between paper signal and our actual close.
        f_close_avg = 0.0
        try:
            slot = pos.flipster_slot
            if slot is not None:
                close_ok = False
                base_px = f_price
                offsets = [0.0, -0.0005, -0.002, -0.005] if side == "long" else [0.0, 0.0005, 0.002, 0.005]
                for off in offsets:
                    try_px = base_px * (1 + off)
                    try:
                        result = self.flipster_client.close_position(
                            flipster_sym, slot, price=try_px,
                        )
                        f_close_avg, _, _ = _extract_flipster_fill(result)
                        print(f"  [FLIPSTER CLOSE] OK @ {f_close_avg} (try ref {try_px:.6f})")
                        close_ok = True
                        break
                    except Exception as e:
                        if "InsufficientLiquidity" not in str(e):
                            raise
                if not close_ok:
                    print(f"  [FLIPSTER CLOSE] all retry prices failed — MANUAL CLOSE NEEDED slot={slot}")
            else:
                print("  [FLIPSTER CLOSE] no slot — manual close needed")
        except Exception as e:
            print(f"  [FLIPSTER CLOSE] FAILED: {e}")

        # Close Gate (reduce_only opposite direction).
        g_close_avg = 0.0
        try:
            g_size = pos.gate_size or gate_size_from_usd(gate_sym, pos.size_usd, g_price)
            close_size = g_size if side == "long" else -g_size
            result = self.gate_client.close_position(
                gate_sym, size=close_size,
            )
            g_close_avg, _ = _extract_gate_fill(result)
            ok = result.get("ok") if isinstance(result, dict) else None
            label = result.get("data", {}).get("label", "") if isinstance(result, dict) else ""
            print(f"  [GATE CLOSE] ok={ok} label={label} size={close_size} fill={g_close_avg}")
        except Exception as e:
            print(f"  [GATE CLOSE] FAILED: {e}")

        # ---- Compute net PnL ----
        # Flipster: long → gain when exit > entry; short → gain when exit < entry
        # Gate hedge is opposite side.
        f_pnl_usd = 0.0
        g_pnl_usd = 0.0
        if pos.flipster_entry_price > 0 and f_close_avg > 0:
            f_dir = 1 if side == "long" else -1
            f_pnl_bp = (f_close_avg - pos.flipster_entry_price) / pos.flipster_entry_price * 1e4 * f_dir
            f_pnl_usd = f_pnl_bp * pos.size_usd / 1e4
        if pos.gate_entry_price > 0 and g_close_avg > 0:
            # Gate: SHORT side when flipster long (so g_dir = -1); LONG when flipster short
            g_dir = -1 if side == "long" else 1
            g_pnl_bp = (g_close_avg - pos.gate_entry_price) / pos.gate_entry_price * 1e4 * g_dir
            # Gate notional ≈ contracts × multiplier × price ≈ size_usd
            spec = GATE_CONTRACTS.get(pos.gate_contract, {"multiplier": 1.0})
            g_notional = pos.gate_size * spec["multiplier"] * pos.gate_entry_price
            g_pnl_usd = g_pnl_bp * g_notional / 1e4

        net_pnl_usd = f_pnl_usd + g_pnl_usd
        # Approx fees: Flipster ~3 bp taker × $size, Gate ~3 bp taker × $size
        approx_fee = (pos.size_usd + (pos.gate_size * GATE_CONTRACTS.get(pos.gate_contract, {"multiplier": 1.0})["multiplier"] * pos.gate_entry_price)) * 0.0003 * 2
        net_after_fees = net_pnl_usd - approx_fee

        self.trade_count += 1
        if net_after_fees > 0:
            self.win_count += 1
        self.total_pnl_usd += net_after_fees

        print(f"  [PNL] flipster=${f_pnl_usd:+.4f} gate=${g_pnl_usd:+.4f} "
              f"net=${net_pnl_usd:+.4f} (-fees ${approx_fee:.4f}) → ${net_after_fees:+.4f}")
        print(f"  [STATS] trades={self.trade_count} wins={self.win_count} "
              f"win%={self.win_count/self.trade_count*100:.1f} total_pnl=${self.total_pnl_usd:+.4f}")

        # Persist to JSONL
        try:
            self.trade_log_path.parent.mkdir(parents=True, exist_ok=True)
            _append_trade(self.trade_log_path, {
                "ts_close": ts,
                "ts_entry": pos.entry_time,
                "pos_id": pos_id,
                "base": base,
                "flipster_side": side,
                "size_usd": pos.size_usd,
                "flipster_entry": pos.flipster_entry_price,
                "flipster_exit": f_close_avg,
                "gate_entry": pos.gate_entry_price,
                "gate_exit": g_close_avg,
                "gate_size": pos.gate_size,
                "gate_contract": pos.gate_contract,
                "f_pnl_usd": round(f_pnl_usd, 6),
                "g_pnl_usd": round(g_pnl_usd, 6),
                "net_pnl_usd": round(net_pnl_usd, 6),
                "approx_fee_usd": round(approx_fee, 6),
                "net_after_fees_usd": round(net_after_fees, 6),
            })
        except Exception as e:
            print(f"  [TRACK] log write failed: {e}")

    def emergency_close_all(self):
        """Close every position currently tracked. Call on shutdown."""
        if not self.open_positions:
            print("[shutdown] no open positions to close")
            return
        print(f"\n[shutdown] EMERGENCY CLOSE: {len(self.open_positions)} open positions")
        for pos_id, pos in list(self.open_positions.items()):
            print(f"  closing pos_id={pos_id} {pos.base} side={pos.flipster_side}")
            # Fake an exit signal
            try:
                # Use last paper price approximation — fall back to entry price
                self._on_exit(pos_id, pos.base, pos.flipster_side, 0.0, 0.0, "shutdown")
            except Exception as e:
                print(f"    err: {e}")

    def run(self):
        print(f"\n{'#'*60}")
        print(f"# Live Executor")
        print(f"# Variant: {self.variant}")
        print(f"# Size: ${self.size_usd:.0f} per trade")
        print(f"# Dry run: {self.dry_run}")
        print(f"# Poll interval: {POLL_INTERVAL}s")
        print(f"{'#'*60}\n")

        # Pre-seed seen IDs from recent signals so we don't replay
        print("[init] Loading recent signals to avoid replay...")
        sql = f"""
            SELECT position_id, action FROM trade_signal
            WHERE account_id = '{self.variant}'
              AND timestamp > dateadd('m', -10, now())
        """
        try:
            for row in query_qdb(sql):
                pid = int(row[0])
                if row[1] == "entry":
                    self.seen_entry_ids.add(pid)
                else:
                    self.seen_exit_ids.add(pid)
            print(f"[init] Pre-seeded {len(self.seen_entry_ids)} entries, "
                  f"{len(self.seen_exit_ids)} exits")
        except Exception as e:
            print(f"[init] Warning: {e}")

        print("[running] Polling for signals... (Ctrl+C to stop)\n")

        while True:
            try:
                self.poll_signals()
            except KeyboardInterrupt:
                break
            except Exception as e:
                print(f"[error] {e}")
            time.sleep(POLL_INTERVAL)

        print(f"\n[stopped] {self.trade_count} trades executed")


# ---------------------------------------------------------------------------
# Setup: start browsers for login
# ---------------------------------------------------------------------------

def setup_browsers():
    print("Starting browsers for login...\n")

    print("=== FLIPSTER ===")
    try:
        from flipster_pkg.client import FlipsterClient
        fc = FlipsterClient()
        url = fc.start_browser()
        print(f"Flipster VNC: {url}")
        print("→ Log in to Flipster, then come back here\n")
    except Exception as e:
        print(f"Flipster browser failed: {e}\n")
        fc = None

    print("=== GATE ===")
    try:
        from gate_pkg.client import GateClient
        gc = GateClient()
        url = gc.start_browser()
        print(f"Gate VNC: {url}")
        print("→ Log in to Gate.io, then come back here\n")
    except Exception as e:
        print(f"Gate browser failed: {e}\n")
        gc = None

    input("Press Enter after logging into BOTH exchanges...")

    if fc:
        try:
            fc.login_done()
            print("✓ Flipster cookies extracted")
        except Exception as e:
            print(f"✗ Flipster cookie extraction failed: {e}")

    if gc:
        try:
            gc.login_done()
            logged_in = gc.is_logged_in()
            print(f"✓ Gate cookies extracted (logged_in={logged_in})")
        except Exception as e:
            print(f"✗ Gate cookie extraction failed: {e}")

    print("\nBrowsers ready. You can now run:")
    print("  python3 scripts/live_executor.py --variant T01_best --size-usd 10")
    print("\nKeep this terminal open (browsers stay alive).")
    print("Press Ctrl+C to stop browsers.")

    try:
        while True:
            time.sleep(60)
    except KeyboardInterrupt:
        pass
    finally:
        if fc:
            fc.stop()
        if gc:
            gc.stop()


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Live executor sidecar")
    parser.add_argument("--setup", action="store_true",
                        help="Start browsers for login")
    parser.add_argument("--variant", type=str, default="T01_best",
                        help="Paper variant to mirror (default: T01_best)")
    parser.add_argument("--size-usd", type=float, default=10.0,
                        help="USD size per trade (default: 10)")
    parser.add_argument("--symbols", type=str, default="",
                        help="Comma-separated symbol whitelist (e.g. BTC,ETH,SOL). Empty = all")
    parser.add_argument("--dry-run", action="store_true",
                        help="Log signals without placing real orders")
    args = parser.parse_args()

    if args.setup:
        setup_browsers()
        return

    # Load Gate contract specs (multipliers needed for correct sizing)
    load_gate_contracts()

    # For live mode, need to connect to already-running browsers
    flipster_client = None
    gate_client = None

    # Load proxies for Flipster (rate-limit avoidance)
    proxies = []
    proxy_file = Path(__file__).resolve().parent / "proxies.txt"
    if proxy_file.exists():
        proxies = [ln.strip() for ln in proxy_file.read_text().splitlines() if ln.strip()]
        print(f"[init] Loaded {len(proxies)} proxies from {proxy_file.name}")

    if not args.dry_run:
        print("[init] Connecting to browser sessions...")
        try:
            from flipster_pkg.client import FlipsterClient
            from flipster_pkg.browser import BrowserManager as FBM
            fc = FlipsterClient(FBM(), proxies=proxies)
            fc.login_done()
            flipster_client = fc
            print(f"  ✓ Flipster connected (proxies={len(proxies)})")
        except Exception as e:
            print(f"  ✗ Flipster: {e}")

        try:
            from gate_pkg.client import GateClient
            from gate_pkg.browser import BrowserManager as GBM
            gc = GateClient(GBM())  # uses default ports (11/5911/6091/9231)
            gc.login_done()
            gate_client = gc
            print(f"  ✓ Gate connected (logged_in={gc.is_logged_in()})")
        except Exception as e:
            print(f"  ✗ Gate: {e}")

        if not flipster_client or not gate_client:
            print("\n✗ One or both exchanges failed to connect. Aborting.")
            return

    whitelist = set(s.strip() for s in args.symbols.split(",") if s.strip()) if args.symbols else set()
    if whitelist:
        print(f"[init] Symbol whitelist: {sorted(whitelist)}")

    executor = Executor(
        variant=args.variant,
        size_usd=args.size_usd,
        dry_run=args.dry_run,
        symbol_whitelist=whitelist,
        flipster_client=flipster_client,
        gate_client=gate_client,
    )

    # Graceful shutdown — close all open positions
    def handle_sig(sig, frame):
        print(f"\n[signal {sig}] Shutting down — closing all open positions...")
        try:
            executor.emergency_close_all()
        except Exception as e:
            print(f"[shutdown] emergency_close_all failed: {e}")
        sys.exit(0)
    signal.signal(signal.SIGINT, handle_sig)
    signal.signal(signal.SIGTERM, handle_sig)

    executor.run()


if __name__ == "__main__":
    main()
