#!/usr/bin/env python3
"""Stream Pyth Hermes price feeds for stocks/futures into QuestDB
table `pyth_bookticker`, in the same shape as our other bookticker
tables so cross-venue analytics queries (ASOF JOIN) just work.

Pyth gives price + confidence; we store mid=price, bid=ask=price
(no real BBO from oracle). Update cadence: 50ms majors, 200ms others.

Usage:
    nohup python3 scripts/pyth_collector.py > logs/pyth_collector.log 2>&1 &
"""
import asyncio
import json
import os
import socket
import time
from datetime import datetime, timezone

import websockets

QDB_ILP_HOST = os.getenv("QDB_HOST", "211.181.122.102")
QDB_ILP_PORT = int(os.getenv("QDB_ILP_PORT", "9009"))
PYTH_WS = "wss://hermes.pyth.network/ws"

# Curated symbol → hermes_id map.
# Stocks + indices we already trade on Lighter.
# NMU6/EMU6/DMU6 are Pyth's own CME futures price feeds (NQ/ES/DJ E-mini).
SYM_IDS = {
    "AAPL":  "241b9a5ce1c3e4bfc68e377158328628f1b478afaa796c4b1760bd3713c2d2d2",
    "AMD":   "7178689d88cdd76574b64438fc57f4e57efaf0bf5f9593ee19c10e46a3c5b5cf",
    "AMZN":  "4ec1330b56eca05037c6b5a51d05f73db79bf3b4d29899881acd27966af184b4",
    "ASML":  "1a2205a8926d5d48ab165f25a58fb758c2422d0d3bcb9b381df3a1afa619e6bd",
    "AVGO":  "4aca602c84fa6f1578f39f9f80c210cddf55e03c7850bf54b190b28c16a4c9bc",
    "GOOGL": "07d24bb76843496a45bce0add8b51555f2ea02098cb04f4c6d61f7b5720836b4",
    "INTC":  "20e8ff9baf410664638c3ef80d091a13088cfcab442458e94642f39182cbff32",
    "META":  "783a457c2fe5642c96a66ba9a2fe61f511e9a0b539e0ed2a443321978e4d65a1",
    "MRVL":  "7aef4e90557add5289266340ccd1e1aa7a225f1220206b07aaf98e53101ce116",
    "MSFT":  "8f98f8267ddddeeb61b4fd11f21dc0c2842c417622b4d685243fa73b5830131f",
    "MU":    "397d361aca271fc93eb9005843d2ec8877e1fa9a12f0809fdf3367b12c3b693a",
    "NVDA":  "c949a96fd1626e82abc5e1496e6e8d44683ac8ac288015ee90bf37257e3e6bf6",
    "ORCL":  "ea4801b45b1d1559414d626beb36325883c4ec3563a16c72b3651951a38365a9",
    "QQQ":   "0eda5e8f3e5881e7e64971b02359250f9d70977e63940c4c9c0d77f54195f13e",
    "SPY":   "05d590e94e9f51abe18ed0421bc302995673156750e914ac1600583fe2e03f99",
    "SNDK":  "bd2c3ac72d7baa967190ed3333e28770cacc6b8a406e9e932a72ac508e3d252d",
    "TSLA":  "713631e41c06db404e6a5d029f3eebfd5b885c59dce4a19f337c024e26584e26",
    # CME E-mini futures (Pyth's USD-denominated price for the futures contract)
    "NMU6":  "3d79439c08ceecb2ecc182dabcb79212d8d8f10780671ef012daecd905923361",
    "EMU6":  "d2b78de5e07d7d29ea1a26a9cfc0a1dd3fb22619616cae0d9ad7debdd49d31e3",
}
ID_TO_SYM = {v: k for k, v in SYM_IDS.items()}


def write_ilp(rows):
    """Write a batch of rows to QuestDB ILP. Each row is a dict."""
    if not rows:
        return
    lines = []
    for r in rows:
        # Use Pyth's publish_time (seconds) as event time, in nanos.
        ts_ns = int(r["publish_time"] * 1_000_000_000)
        line = (
            f"pyth_bookticker,symbol={r['sym']} "
            f"bid_price={r['mid']:.6f},"
            f"ask_price={r['mid']:.6f},"
            f"bid_size=0,"
            f"ask_size=0,"
            f"mark_price={r['mid']:.6f},"
            f"index_price={r['mid']:.6f},"
            f"conf={r['conf']:.6f}"
            f" {ts_ns}"
        )
        lines.append(line)
    payload = ("\n".join(lines) + "\n").encode()
    try:
        with socket.create_connection((QDB_ILP_HOST, QDB_ILP_PORT), timeout=5) as sock:
            sock.sendall(payload)
    except Exception as e:
        print(f"[ilp] err: {e}", flush=True)


async def run():
    print(f"pyth_collector: {len(SYM_IDS)} feeds → {QDB_ILP_HOST}:{QDB_ILP_PORT}", flush=True)
    while True:
        try:
            async with websockets.connect(PYTH_WS, max_size=2**24) as ws:
                await ws.send(json.dumps({
                    "type": "subscribe",
                    "ids": list(SYM_IDS.values()),
                }))
                print("subscribed.", flush=True)
                buf = []
                last_flush = time.time()
                last_print = 0
                msg_count = 0
                while True:
                    raw = await ws.recv()
                    msg_count += 1
                    try:
                        m = json.loads(raw)
                    except Exception:
                        continue
                    if m.get("type") != "price_update":
                        continue
                    pf = m.get("price_feed") or {}
                    pid = pf.get("id")
                    p = pf.get("price") or {}
                    sym = ID_TO_SYM.get(pid)
                    if not sym:
                        continue
                    try:
                        px = float(p["price"]) * (10 ** p["expo"])
                        conf = float(p.get("conf", 0)) * (10 ** p["expo"])
                        pub = int(p["publish_time"])
                    except Exception:
                        continue
                    if px <= 0:
                        continue
                    buf.append({"sym": sym, "mid": px, "conf": conf, "publish_time": pub})
                    # Flush in batches every ~250ms or 100 rows.
                    now = time.time()
                    if len(buf) >= 100 or now - last_flush > 0.25:
                        write_ilp(buf)
                        buf.clear()
                        last_flush = now
                    # Periodic stdout heartbeat.
                    if now - last_print > 30:
                        print(f"[hb] msgs={msg_count}", flush=True)
                        last_print = now
        except Exception as e:
            print(f"[ws] reconnect after err: {e}", flush=True)
            await asyncio.sleep(2)


if __name__ == "__main__":
    asyncio.run(run())
