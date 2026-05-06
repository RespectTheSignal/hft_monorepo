#!/usr/bin/env python3
"""BingX spread_revert signal generator.

Subscribes to Binance futures WS (reference) + BingX public WS (trade venue).
Maintains 30-min rolling baseline of (bingx_mid - binance_mid) per coin.
Emits ZMQ trade_signal frames consumed by `bingx_executor`.

Env (all optional):
  SR_PUB_ADDR        ipc:///tmp/bingx_sr_signal.sock
  SR_ACCOUNT_ID      BINGX_SR_v1
  SR_SIZE_USD        20
  SR_ENTRY_BP        2.0    extra threshold over costs
  SR_STOP_BP         20     adverse single-leg move
  SR_MAX_HOLD_S      1800   timeout (30 min)
  SR_COOLDOWN_S      30     per-coin cooldown
  SR_BASELINE_MIN    30     rolling window for baseline
  SR_WHITELIST       "LAB,TAO,GALA,...,MASK"  (comma-sep, default = top 25)
"""
import asyncio, json, time, uuid, os, sys
from collections import deque, defaultdict
from datetime import datetime, timezone
import websockets, gzip, zmq

DEFAULT_WHITELIST = "LAB,TAO,GALA,LINK,M,IRYS,PIXEL,GMX,SAPIEN,MAVIA,BNB,MERL,MOODENG,ORCA,BTC,TNSR,QNT,LQTY,ETHFI,MORPHO,EPIC,GPS,SKR,CYS,MASK"

ACCOUNT_ID    = os.environ.get("SR_ACCOUNT_ID", "BINGX_SR_v1")
SIZE_USD      = float(os.environ.get("SR_SIZE_USD", "20"))
ENTRY_BP      = float(os.environ.get("SR_ENTRY_BP", "2.0"))
STOP_BP       = float(os.environ.get("SR_STOP_BP", "20"))
MAX_HOLD_S    = float(os.environ.get("SR_MAX_HOLD_S", "1800"))
COOLDOWN_S    = float(os.environ.get("SR_COOLDOWN_S", "30"))
BASELINE_MIN  = int(os.environ.get("SR_BASELINE_MIN", "30"))
WHITELIST     = os.environ.get("SR_WHITELIST", DEFAULT_WHITELIST).split(",")
PUB_ADDR      = os.environ.get("SR_PUB_ADDR", "ipc:///tmp/bingx_sr_signal.sock")

BINANCE_WS = "wss://fstream.binance.com/ws"
BINGX_WS   = "wss://open-api-swap.bingx.com/swap-market"

# ── ZMQ pub ──
ctx = zmq.Context()
pub = ctx.socket(zmq.PUB); pub.setsockopt(zmq.LINGER, 100); pub.bind(PUB_ADDR)
print(f"[sr] PUB bound: {PUB_ADDR}")

# ── State ──
class S:
    bin_mid = None
    bx_mid  = None
    bx_bid  = None
    bx_ask  = None
    history = None       # deque of gap_bp samples (1 per second)
    last_sample_t = 0.0
    last_signal_t = 0.0
    open_pos = None      # dict {side, entry_px, entry_t, mid_at_entry, position_id}

state = defaultdict(S)
for c in WHITELIST:
    state[c].history = deque(maxlen=BASELINE_MIN * 60)

next_pid = 1
def alloc_pid():
    global next_pid
    v = next_pid; next_pid += 1
    return v

def now_iso():
    return datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%S.%fZ')

async def publish(sig):
    # NO spaces in separators — executor uses substring filter
    # `"account_id":"<variant>"` to filter by variant.
    pub.send(json.dumps(sig, separators=(",", ":")).encode(), zmq.DONTWAIT)

async def consider(coin):
    """Run on every BingX tick — manage open position + check entry."""
    s = state[coin]
    if s.bin_mid is None or s.bx_mid is None or s.bx_bid is None: return

    # Update history (every second)
    t = time.time()
    if t - s.last_sample_t >= 1.0:
        gap = (s.bx_mid - s.bin_mid) / s.bin_mid * 1e4
        s.history.append(gap)
        s.last_sample_t = t
    if len(s.history) < 600:    # need at least 10 min before any signal
        return

    baseline = sorted(s.history)[len(s.history)//2]   # median
    current_gap = (s.bx_mid - s.bin_mid) / s.bin_mid * 1e4
    dev = current_gap - baseline
    spread_bp = (s.bx_ask - s.bx_bid) / s.bx_mid * 1e4 if s.bx_mid else 0
    threshold = (spread_bp/2) + ENTRY_BP

    # ── Manage open ──
    if s.open_pos is not None:
        op = s.open_pos
        elapsed = t - op['entry_t']
        if op['side'] == 'long':
            adverse_bp = (op['entry_px'] - s.bx_ask) / op['mid'] * 1e4
        else:
            adverse_bp = (s.bx_bid - op['entry_px']) / op['mid'] * 1e4
        stop = adverse_bp >= STOP_BP
        timeout = elapsed >= MAX_HOLD_S
        reverse = (op['side'] == 'long' and dev > threshold) or \
                  (op['side'] == 'short' and dev < -threshold)
        if stop or timeout or reverse:
            reason = 'stop' if stop else ('reverse' if reverse else 'timeout')
            exit_price = s.bx_bid if op['side'] == 'long' else s.bx_ask
            await publish({
                "account_id": ACCOUNT_ID, "base": coin, "action": "exit",
                "side": op['side'], "size_usd": SIZE_USD,
                "flipster_price": s.bx_mid, "gate_price": s.bin_mid,
                "position_id": op['position_id'], "timestamp": now_iso(),
            })
            print(f"[sr] EXIT {coin} {op['side']:>5} {reason:<8} entry={op['entry_px']:.6g} exit={exit_price:.6g} "
                  f"gross_bp={(exit_price-op['entry_px'])/op['mid']*1e4*(1 if op['side']=='long' else -1):+.2f} held={elapsed:.0f}s pid={op['position_id']}")
            s.open_pos = None
            s.last_signal_t = t
            return
        return  # in position, no entry

    # ── Entry ──
    if t - s.last_signal_t < COOLDOWN_S:
        return
    side = None
    if dev > threshold:
        side = 'short'; entry_px = s.bx_bid
    elif dev < -threshold:
        side = 'long'; entry_px = s.bx_ask
    else:
        return
    pid = alloc_pid()
    s.open_pos = {
        'side': side, 'entry_px': entry_px, 'entry_t': t,
        'mid': s.bx_mid, 'position_id': pid,
    }
    s.last_signal_t = t
    await publish({
        "account_id": ACCOUNT_ID, "base": coin, "action": "entry",
        "side": side, "size_usd": SIZE_USD,
        "flipster_price": s.bx_mid, "gate_price": s.bin_mid,
        "position_id": pid, "timestamp": now_iso(),
        "flipster_bid": s.bx_bid, "flipster_ask": s.bx_ask,
    })
    print(f"[sr] ENTRY {coin} {side:>5} dev={dev:+.2f}bp baseline={baseline:+.2f} thresh={threshold:.2f} "
          f"px={entry_px:.6g} pid={pid}")

# ── Binance subscriber ──
async def binance_ws():
    while True:
        try:
            async with websockets.connect(BINANCE_WS, ping_interval=20) as ws:
                params = [f"{c.lower()}usdt@bookTicker" for c in WHITELIST]
                await ws.send(json.dumps({"method": "SUBSCRIBE", "params": params, "id": 1}))
                async for m in ws:
                    j = json.loads(m)
                    if 'b' in j and 's' in j:
                        sym = j['s']
                        if not sym.endswith('USDT'): continue
                        coin = sym[:-4]
                        if coin not in state: continue
                        try:
                            bid = float(j['b']); ask = float(j['a'])
                            state[coin].bin_mid = (bid + ask) / 2
                        except: pass
        except Exception as e:
            print(f"[sr] binance WS reconnect: {e}")
            await asyncio.sleep(2)

# ── BingX subscriber ──
async def bingx_ws():
    while True:
        try:
            async with websockets.connect(BINGX_WS, ping_interval=20) as ws:
                for c in WHITELIST:
                    await ws.send(json.dumps({
                        "id": str(uuid.uuid4()), "reqType": "sub",
                        "dataType": f"{c}-USDT@bookTicker"
                    }))
                    await asyncio.sleep(0.02)
                while True:
                    m = await ws.recv()
                    if isinstance(m, bytes):
                        try: m = gzip.decompress(m).decode()
                        except: continue
                    if m == 'Ping':
                        await ws.send('Pong'); continue
                    try: j = json.loads(m)
                    except: continue
                    if 'ping' in j:
                        await ws.send(json.dumps({'pong': j['ping']})); continue
                    dt = j.get('dataType', '')
                    if '@bookTicker' not in dt: continue
                    coin = dt.split('@')[0].replace('-USDT', '')
                    if coin not in state: continue
                    d = j.get('data') or {}
                    try:
                        bid = float(d.get('b'))
                        ask = float(d.get('a'))
                        s = state[coin]
                        s.bx_bid = bid; s.bx_ask = ask; s.bx_mid = (bid + ask) / 2
                        await consider(coin)
                    except: pass
        except Exception as e:
            print(f"[sr] bingx WS reconnect: {e}")
            await asyncio.sleep(2)

async def main():
    print(f"[sr] starting — whitelist={len(WHITELIST)} coins  size=${SIZE_USD}  entry={ENTRY_BP}bp  stop={STOP_BP}bp")
    print(f"[sr] coins: {','.join(WHITELIST)}")
    await asyncio.gather(binance_ws(), bingx_ws())

if __name__ == "__main__":
    asyncio.run(main())
