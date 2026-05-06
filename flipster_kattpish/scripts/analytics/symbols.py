"""Symbol normalization across exchanges.

Returns a canonical "base" coin (BTC, ETH, SOL, ...) given an exchange-specific symbol.
Returns None if the symbol is not a USD/USDT/USDC perpetual.
"""
import re

# Exchange-specific token aliases for the same underlying coin.
# Map normalized -> canonical.
ALIASES = {
    "XBT": "BTC",   # kraken
}

def _canon(base: str) -> str:
    base = base.upper()
    return ALIASES.get(base, base)

def _strip_quote(s: str) -> str | None:
    """Strip USDT/USDC/USD/PERP/SWAP suffixes; return base or None if not a USD-quoted perp."""
    s = s.upper().replace("-", "_").replace("/", "_")
    # Remove well-known suffixes in order
    for suf in (".PERP", "_PERP", "_SWAP", "M"):  # kucoin uses suffix M for perp
        # M only at very end, only after USDT
        if suf == "M" and s.endswith("USDTM"):
            s = s[:-1]  # drop trailing M; "BTCUSDTM" -> "BTCUSDT"
        elif suf != "M" and s.endswith(suf):
            s = s[: -len(suf)]
    # Now s should end with _USDT, _USDC, _USD or no underscore (e.g. "BTCUSDT", "BTC")
    # Try with underscore first
    for q in ("_USDT", "_USDC", "_USD"):
        if s.endswith(q):
            return s[: -len(q)]
    # No underscore — try concat suffix
    for q in ("USDT", "USDC", "USD"):
        if s.endswith(q) and len(s) > len(q):
            return s[: -len(q)]
    # hyperliquid uses bare base (e.g. "BTC")
    if "_" not in s and s.isalnum():
        return s
    return None

# Per-exchange overrides where the generic logic fails
def _norm_kraken(symbol: str) -> str | None:
    # PF_XBTUSD, PF_PENGUUSD, PI_BTCUSD (inverse)
    if not symbol.startswith("PF_"):
        return None  # only linear PF_ contracts
    body = symbol[3:]  # XBTUSD, PENGUUSD
    if body.endswith("USD"):
        return _canon(body[:-3])
    return None

def _norm_pyth(symbol: str) -> str | None:
    # BTC/USD, ETH/USD
    if "/" not in symbol:
        return None
    base, quote = symbol.split("/", 1)
    if quote.upper() not in ("USD", "USDT", "USDC"):
        return None
    return _canon(base)

def _norm_okx(symbol: str) -> str | None:
    # BTC-USDT-SWAP
    parts = symbol.split("-")
    if len(parts) == 3 and parts[2].upper() == "SWAP" and parts[1].upper() in ("USDT","USDC","USD"):
        return _canon(parts[0])
    return None

def _norm_backpack(symbol: str) -> str | None:
    # BTC_USDC_PERP
    parts = symbol.split("_")
    if len(parts) == 3 and parts[2].upper() == "PERP" and parts[1].upper() in ("USDT","USDC","USD"):
        return _canon(parts[0])
    return None

def _norm_kucoin(symbol: str) -> str | None:
    # XBTUSDTM, BTCUSDTM, ETHUSDTM (trailing M = futures)
    if not symbol.endswith("M"):
        return None
    s = symbol[:-1]  # XBTUSDT
    for q in ("USDT", "USDC", "USD"):
        if s.endswith(q):
            return _canon(s[: -len(q)])
    return None

def _norm_hyperliquid(symbol: str) -> str | None:
    # bare "BTC", "ETH", "@123" (internal)
    if symbol.startswith("@") or "_" in symbol or "-" in symbol:
        return None
    return _canon(symbol)

def _norm_underscore_usdt(symbol: str) -> str | None:
    # binance/bitget/bybit/mexc/gate: "BTC_USDT"
    if "_" not in symbol:
        return None
    parts = symbol.split("_")
    if len(parts) >= 2 and parts[-1].upper() in ("USDT","USDC","USD"):
        return _canon(parts[0])
    return None

def _norm_dash_usdt(symbol: str) -> str | None:
    # bingx: "BTC-USDT"
    if "-" not in symbol:
        return None
    parts = symbol.split("-")
    if len(parts) == 2 and parts[1].upper() in ("USDT","USDC","USD"):
        return _canon(parts[0])
    return None

NORMALIZERS = {
    "binance":     _norm_underscore_usdt,
    "bitget":      _norm_underscore_usdt,
    "bybit":       _norm_underscore_usdt,
    "mexc":        _norm_underscore_usdt,
    "gate":        _norm_underscore_usdt,
    "bingx":       _norm_dash_usdt,
    "okx":         _norm_okx,
    "backpack":    _norm_backpack,
    "kucoin":      _norm_kucoin,
    "kraken":      _norm_kraken,
    "hyperliquid": _norm_hyperliquid,
    "pyth":        _norm_pyth,
}

# Stocks/commodities/options noise patterns to filter out
_NON_CRYPTO_PATTERNS = [
    re.compile(r"^NC[A-Z0-9]+$"),     # bingx stock tickers like NCSKAAPL2USD
    re.compile(r"^NCCO\d+OIL"),        # commodities
    re.compile(r".*OIL$"),             # USOIL, UKOIL, BRENT_OIL etc
    re.compile(r".*STOCK$"),           # TSMSTOCK
    re.compile(r"^TSTBSC$"),           # test tokens
    re.compile(r"^TST$"),
    re.compile(r"^\d+$"),              # numeric-only like "4"
]

def normalize(exchange: str, symbol: str) -> str | None:
    """Return canonical base coin (e.g. 'BTC') for (exchange, symbol), or None."""
    fn = NORMALIZERS.get(exchange)
    if fn is None:
        return None
    base = fn(symbol)
    if base is None:
        return None
    if any(p.match(base) for p in _NON_CRYPTO_PATTERNS):
        return None
    if len(base) > 20:  # too long, probably noise
        return None
    return base


if __name__ == "__main__":
    test_cases = [
        ("binance", "BTC_USDT", "BTC"),
        ("binance", "USOIL_USDT", None),
        ("bitget",  "ETH_USDT", "ETH"),
        ("bingx",   "BTC-USDT", "BTC"),
        ("bingx",   "NCSKAAPL2USD-USDT", None),
        ("okx",     "SOL-USDT-SWAP", "SOL"),
        ("backpack","BTC_USDC_PERP", "BTC"),
        ("kucoin",  "XBTUSDTM", "BTC"),
        ("kucoin",  "ETHUSDTM", "ETH"),
        ("kraken",  "PF_XBTUSD", "BTC"),
        ("kraken",  "PF_PENGUUSD", "PENGU"),
        ("hyperliquid", "BTC", "BTC"),
        ("hyperliquid", "@123", None),
        ("pyth",    "BTC/USD", "BTC"),
        ("pyth",    "GLM/USD", "GLM"),
    ]
    fail = 0
    for ex, sym, want in test_cases:
        got = normalize(ex, sym)
        ok = "OK" if got == want else "FAIL"
        if got != want: fail += 1
        print(f"  [{ok}] {ex:12s} {sym:30s} -> {got!r}  (want {want!r})")
    print(f"{fail} failures")
