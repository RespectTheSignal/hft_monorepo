"""Effective fees per exchange (perp futures, basic tier, in basis points).

Flipster Banner discounts (payback %) applied to maker/taker baseline.
For exchanges not on the banner, default rates from public fee schedules (2026).
Excluded: flipster (no data window), pyth (oracle), hashkey (sparse).
"""

# raw_taker_pct, raw_maker_pct, payback_pct (0..1)
RAW = {
    "binance":     (0.0400, 0.0160, 0.20),
    "bybit":       (0.0308, 0.0140, 0.30),
    "okx":         (0.0225, 0.0090, 0.55),
    "bitget":      (0.0180, 0.0090, 0.55),
    "gate":        (0.0150, 0.0060, 0.70),
    # bingx: real raw 0.05% taker / 0.02% maker, Banner net 32% paid (68% off).
    # The earlier "0.02%/0.008% × 60% payback" reading was an underestimate —
    # the banner shows the *post-discount effective* not the raw. Verified live.
    "bingx":       (0.0500, 0.0200, 0.68),
    "backpack":    (0.04875, 0.0390, 0.35),
    "mexc":        (0.0200, 0.0000, 0.00),
    "kucoin":      (0.0600, 0.0200, 0.00),
    "kraken":      (0.0500, 0.0200, 0.00),
    "hyperliquid": (0.0450, 0.0150, 0.00),
}

def fees_bp(exchange: str) -> dict:
    t, m, p = RAW[exchange]
    return {
        "taker_bp": t * (1 - p) * 100,
        "maker_bp": m * (1 - p) * 100,
    }

def round_trip_bp(ex_a: str, ex_b: str, mode: str = "taker_taker") -> float:
    """Round-trip cost for a pair trade: enter both sides, exit both sides."""
    a = fees_bp(ex_a)
    b = fees_bp(ex_b)
    if mode == "taker_taker":
        return 2 * (a["taker_bp"] + b["taker_bp"])
    if mode == "maker_taker":  # 50% maker fill assumption: avg of taker_taker and maker_maker
        return (a["taker_bp"] + b["taker_bp"]) + (a["maker_bp"] + b["maker_bp"])
    if mode == "maker_maker":
        return 2 * (a["maker_bp"] + b["maker_bp"])
    raise ValueError(mode)

if __name__ == "__main__":
    print(f"{'exchange':12s} {'taker_bp':>9s} {'maker_bp':>9s}")
    for ex in RAW:
        f = fees_bp(ex)
        print(f"{ex:12s} {f['taker_bp']:9.4f} {f['maker_bp']:9.4f}")
    print()
    print("Sample round-trip taker_taker:")
    samples = [("gate","binance"),("gate","bitget"),("binance","mexc"),("hyperliquid","gate"),("kraken","binance")]
    for a,b in samples:
        print(f"  {a:12s} × {b:12s} {round_trip_bp(a,b):.3f} bp")
