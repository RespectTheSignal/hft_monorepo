"""One-shot position dump for both exchanges. Re-uses the kattpish browsers/cookies."""
import sys
import importlib.util
from pathlib import Path
import json

ROOT = Path(__file__).resolve().parent.parent
_QUANT = ROOT.parent
MONOREPO = _QUANT / "hft_monorepo" if (_QUANT / "hft_monorepo" / "flipster_web").exists() else _QUANT


def _load_pkg(name: str, root: Path):
    pkg_dir = root / "python"
    spec = importlib.util.spec_from_file_location(
        name, pkg_dir / "__init__.py", submodule_search_locations=[str(pkg_dir)]
    )
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


flipster_pkg = _load_pkg("flipster_pkg", MONOREPO / "flipster_web")
gate_pkg = _load_pkg("gate_pkg", MONOREPO / "gate_web")
from gate_pkg.client import GateClient, API_BASE as GATE_BASE
from flipster_pkg.client import FlipsterClient, API_BASE as FLIP_BASE

# ---- Gate ----
gc = GateClient()
gc.start_browser()
gc.login_done()
print("\n========= GATE positions =========")
url = f"{GATE_BASE}/apiw/v2/futures/usdt/positions"
try:
    r = gc._session.get(url, timeout=10)
    data = r.json()
    items = data.get("data") if isinstance(data, dict) else data
    if isinstance(items, dict):
        items = items.get("data", items)
    if not isinstance(items, list):
        print("Raw response:", json.dumps(data, indent=2)[:600])
        items = []
    open_pos = [p for p in items if int(p.get("size", 0)) != 0]
    print(f"  total={len(items)}  open(non-zero)={len(open_pos)}")
    for p in open_pos:
        sz = int(p.get("size", 0))
        side = "long" if sz > 0 else "short"
        contract = p.get("contract")
        entry = p.get("entry_price")
        mark = p.get("mark_price")
        upnl = p.get("unrealised_pnl")
        rpnl = p.get("realised_pnl")
        margin = p.get("margin")
        print(f"  {contract:>15} {side:>5} size={sz:>6} entry={entry} mark={mark} uPnL={upnl} rPnL={rpnl} margin={margin}")
except Exception as e:
    print("Gate fetch failed:", e)

# ---- Flipster ----
fc = FlipsterClient()
fc.start_browser()
fc.login_done()
print("\n========= FLIPSTER positions =========")
url = f"{FLIP_BASE}/api/v2/trade/positions"
try:
    r = fc._session.get(url, timeout=10)
    try:
        data = r.json()
    except Exception:
        print("non-JSON:", r.text[:300])
        data = {}
    items = data
    if isinstance(data, dict):
        items = data.get("positions") or data.get("data") or data.get("items") or []
    if not isinstance(items, list):
        print("Raw response:", json.dumps(data, indent=2)[:1000])
        items = []
    open_pos = [p for p in items if abs(float(p.get("size") or 0)) > 0]
    print(f"  total={len(items)}  open(non-zero)={len(open_pos)}")
    for p in open_pos:
        sym = p.get("symbol")
        sz = float(p.get("size") or 0)
        side = p.get("side") or ("long" if sz > 0 else "short")
        avg = p.get("avgPrice") or p.get("entryPrice")
        mark = p.get("markPrice")
        upnl = p.get("unrealizedPnl") or p.get("unrealisedPnL")
        margin = p.get("margin")
        slot = p.get("slot")
        print(f"  {sym:>20} {side:>5} size={sz:>10} entry={avg} mark={mark} uPnL={upnl} margin={margin} slot={slot}")
except Exception as e:
    print("Flipster fetch failed:", e)

print("\ndone.")
