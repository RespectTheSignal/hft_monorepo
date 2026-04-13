"""Resilience 테스트 — 인프로세스 fault injection.

외부 의존 없음. 다음 시나리오를 검증:
  1. Aggregator가 MockFeed의 None/예외에도 살아남고 재연결 시도
  2. ZMQ feed: 로컬 PUB 재시작 후 메시지 수신 복구
  3. WS client: 로컬 WS 서버가 N초 후 연결 종료 → 재연결 + on_reconnect 호출
  4. REST 재시도: 로컬 HTTP 서버가 K회 503 → 이후 200 → 성공

실행:
    uv run python scripts/test_resilience.py
"""

from __future__ import annotations

import asyncio
import json
import struct
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

import httpx
import websockets

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.market_data.aggregator import MarketDataAggregator
from strategy_flipster.market_data.zmq_feed import ExchangeZmqFeed
from strategy_flipster.types import BookTicker
from strategy_flipster.user_data.state import UserState
from strategy_flipster.user_data.ws_client import FlipsterUserWsClient

FAIL: list[str] = []


def check(name: str, cond: bool, detail: str = "") -> None:
    tag = "✓" if cond else "✗"
    print(f"  {tag} {name}{(' — ' + detail) if detail else ''}")
    if not cond:
        FAIL.append(name)


def hr(title: str) -> None:
    print(f"\n{'=' * 6} {title} {'=' * (60 - len(title))}")


# ── 시나리오 1: Aggregator가 None/예외에 내성 ─────────────

class MockFeed:
    """첫 N번 None 반환 → 예외 1번 → 정상 BookTicker 반복"""

    def __init__(self) -> None:
        self.connect_count = 0
        self.disconnect_count = 0
        self.recv_count = 0

    async def connect(self) -> None:
        self.connect_count += 1

    async def disconnect(self) -> None:
        self.disconnect_count += 1

    async def recv(self) -> BookTicker | None:
        self.recv_count += 1
        n = self.recv_count
        if n == 1:
            return None  # 연결 종료 신호 → 재연결 유도
        if n == 2:
            raise RuntimeError("simulated transient error")
        # 이후는 정상
        return BookTicker(
            exchange="mock",
            symbol="TEST",
            bid_price=100.0 + n,
            ask_price=100.1 + n,
            bid_size=1.0,
            ask_size=1.0,
            last_price=0.0,
            mark_price=0.0,
            index_price=0.0,
            event_time_ms=int(time.time() * 1000),
            recv_ts_ns=time.time_ns(),
        )


async def test_aggregator_resilience() -> None:
    hr("1. Aggregator: MockFeed None/예외 내성")
    feed = MockFeed()
    agg = MarketDataAggregator([feed], queue_size=100)
    await agg.start()
    try:
        received: list[BookTicker] = []
        for _ in range(3):
            t = await asyncio.wait_for(agg.recv(), timeout=5.0)
            received.append(t)
        check("3개 tick 수신", len(received) == 3)
        check("재연결 호출됨 (connect_count ≥ 2)", feed.connect_count >= 2, f"connect_count={feed.connect_count}")
        check("disconnect 호출됨", feed.disconnect_count >= 1, f"disconnect_count={feed.disconnect_count}")
    finally:
        await agg.stop()


# ── 시나리오 2: ZMQ 로컬 PUB 재시작 복구 ─────────────

def _make_bookticker_frame() -> bytes:
    """ExchangeZmqFeed가 파싱할 수 있는 120B payload + 프레이밍"""
    import struct as _s
    exchange = b"binance".ljust(16, b"\x00")
    symbol = b"BTCUSDT".ljust(32, b"\x00")
    payload = (
        exchange + symbol
        + _s.pack("<4d", 50000.0, 50001.0, 1.0, 1.0)
        + _s.pack("<5q", int(time.time() * 1000), int(time.time() * 1000), 0, 0, 0)
    )
    assert len(payload) == 120
    type_str = b"bookticker"
    return bytes([len(type_str)]) + type_str + payload


async def test_zmq_reconnect() -> None:
    hr("2. ZMQ: 로컬 PUB 재시작 후 복구")
    import zmq
    import zmq.asyncio

    address = "tcp://127.0.0.1:15999"
    ctx = zmq.asyncio.Context()

    async def run_publisher(stop_event: asyncio.Event) -> None:
        pub = ctx.socket(zmq.PUB)
        pub.bind(address)
        await asyncio.sleep(0.3)  # subscriber slow-join buffer
        while not stop_event.is_set():
            try:
                await pub.send(_make_bookticker_frame())
            except Exception:
                break
            await asyncio.sleep(0.05)
        pub.close()

    feed = ExchangeZmqFeed(zmq_address=address, exchange_name="binance")
    agg = MarketDataAggregator([feed], queue_size=100)
    await agg.start()

    stop1 = asyncio.Event()
    pub_task1 = asyncio.create_task(run_publisher(stop1))

    received_before: list[BookTicker] = []
    try:
        for _ in range(3):
            t = await asyncio.wait_for(agg.recv(), timeout=5.0)
            received_before.append(t)
        check("PUB 최초 구동 후 3 tick 수신", len(received_before) == 3)

        # 퍼블리셔 종료
        stop1.set()
        await pub_task1

        # 재시작 (동일 주소)
        stop2 = asyncio.Event()
        pub_task2 = asyncio.create_task(run_publisher(stop2))
        received_after: list[BookTicker] = []
        # ZMQ SUB은 내부 재연결하므로 stale timeout 없이도 수신 가능
        for _ in range(3):
            t = await asyncio.wait_for(agg.recv(), timeout=10.0)
            received_after.append(t)
        check("PUB 재시작 후 3 tick 수신", len(received_after) == 3)

        stop2.set()
        await pub_task2
    finally:
        await agg.stop()
        ctx.term()


# ── 시나리오 3: WS 서버 강제 종료 후 재연결 + on_reconnect 콜백 ─────────────

async def test_ws_reconnect() -> None:
    hr("3. WS: 로컬 서버 종료 → 재연결 → on_reconnect 호출")

    server_close_after = 2  # N개 메시지 전달 후 강제 종료
    server_connects = 0
    resync_calls = 0

    async def handler(ws: websockets.ServerConnection) -> None:
        nonlocal server_connects
        server_connects += 1
        try:
            await ws.recv()  # subscribe
            for i in range(server_close_after):
                # account snapshot 토픽 응답
                await ws.send(json.dumps({
                    "topic": "account",
                    "data": [{"rows": [{
                        "totalWalletBalance": f"{1000 + i}",
                        "totalUnrealizedPnl": "0",
                        "totalMarginBalance": f"{1000 + i}",
                        "availableBalance": f"{1000 + i}",
                    }]}],
                }))
                await asyncio.sleep(0.1)
        finally:
            await ws.close()

    server = await websockets.serve(handler, "127.0.0.1", 15998)
    try:
        ws_url = "ws://127.0.0.1:15998"
        config = FlipsterApiConfig(api_key="x", api_secret="y", ws_url=ws_url)
        state = UserState()

        async def on_reconnect() -> None:
            nonlocal resync_calls
            resync_calls += 1

        client = FlipsterUserWsClient(config, state, on_reconnect=on_reconnect)
        client._max_reconnect_delay = 1.0  # 테스트 가속

        task = asyncio.create_task(client.start())
        # 2번 연결될 때까지 대기 (최초 + 재연결 1회)
        t0 = time.monotonic()
        while server_connects < 2 and time.monotonic() - t0 < 10.0:
            await asyncio.sleep(0.1)
        check("서버 2회 연결 관찰", server_connects >= 2, f"connects={server_connects}")
        # 재연결 후 on_reconnect 호출까지 잠깐 대기
        await asyncio.sleep(0.5)
        check("on_reconnect 최소 1회 호출", resync_calls >= 1, f"resync_calls={resync_calls}")

        await client.stop()
        task.cancel()
        try:
            await asyncio.wait_for(task, timeout=2.0)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            pass
    finally:
        server.close()
        await server.wait_closed()


# ── 시나리오 4: REST 503 재시도 후 성공 ─────────────

async def test_rest_retry() -> None:
    hr("4. REST: 503 K회 후 200")
    from aiohttp import web  # type: ignore[import-not-found]

    call_count = 0
    fail_first = 2

    async def ticker_handler(request: "web.Request") -> "web.Response":
        nonlocal call_count
        call_count += 1
        if call_count <= fail_first:
            return web.Response(status=503, text='{"error":"service_unavailable"}')
        return web.json_response([{
            "symbol": "BTCUSDT.PERP",
            "bidPrice": "50000", "askPrice": "50001",
            "lastPrice": "50000.5", "markPrice": "50000.2",
            "indexPrice": "50000.1", "fundingRate": "0",
            "nextFundingTime": 0, "openInterest": "100",
            "volume24h": "1000", "turnover24h": "50000000", "priceChange24h": "100",
        }])

    app = web.Application()
    app.router.add_get("/api/v1/market/ticker", ticker_handler)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 15997)
    await site.start()

    try:
        config = FlipsterApiConfig(
            api_key="x", api_secret="y",
            base_url="http://127.0.0.1:15997",
        )
        client = FlipsterExecutionClient(config, max_retries=3, backoff_base=0.05)
        await client.start()
        try:
            t0 = time.monotonic()
            tickers = await client.get_all_tickers()
            elapsed = time.monotonic() - t0
            check("503 재시도 후 성공", len(tickers) == 1)
            check(f"총 호출 {fail_first + 1}회 (3회)", call_count == fail_first + 1, f"call_count={call_count}")
            check("백오프 시간 준수 (> 0.1s)", elapsed > 0.1, f"{elapsed:.3f}s")
        finally:
            await client.stop()
    finally:
        await runner.cleanup()


async def main() -> None:
    await test_aggregator_resilience()
    await test_zmq_reconnect()
    await test_ws_reconnect()
    try:
        await test_rest_retry()
    except ModuleNotFoundError as e:
        print(f"\n  ! test_rest_retry 건너뜀 (aiohttp 미설치): {e}")

    hr("결과 요약")
    if FAIL:
        print(f"  실패 {len(FAIL)}개:")
        for name in FAIL:
            print(f"    - {name}")
        sys.exit(1)
    print("  전체 통과")


if __name__ == "__main__":
    asyncio.run(main())
