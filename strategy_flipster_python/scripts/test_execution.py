"""Flipster 실주문 E2E 검증 — BTCUSDT.PERP 최소 수량 LIMIT 주문 + 취소.

환경변수 필요:
    FLIPSTER_API_KEY, FLIPSTER_API_SECRET

실행:
    PYTHONPATH=src python scripts/test_execution.py

동작:
    1. 계정/잔고/포지션 스냅샷 (REST)
    2. 기존 BTCUSDT.PERP 대기 주문 확인 (있으면 경고만)
    3. contract_info + ticker 조회 → mid * 0.9 가격 계산 (BUY LIMIT, 체결 불가능 수준)
    4. WS private 스트림 병행 시작 (이벤트 카운터 + 타임라인 기록)
    5. LIMIT 주문 제출 → orderId 기록 + 제출 레이턴시 측정
    6. REST 폴링으로 pending_orders에 나타나는지 확인 (최대 3초)
    7. 2초 대기 (WS 이벤트 수집)
    8. cancel_order(orderId)
    9. REST 폴링으로 사라진 것 확인
    10. 최종 UserState (WS) vs REST 계정 스냅샷 비교 + 결과 요약

주문은 mid * 0.9 에 제출되므로 정상 시장 상황에서는 체결되지 않음.
종료 시 미취소 주문이 남아있으면 강제 cancel 시도.
"""

from __future__ import annotations

import asyncio
import os
import sys
import time
from decimal import ROUND_DOWN, Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.error import AppException
from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.types import (
    MarginType,
    OrderRequest,
    OrderSide,
    OrderType,
    TimeInForce,
)
from strategy_flipster.user_data.rest_client import FlipsterUserRestClient
from strategy_flipster.user_data.state import UserState
from strategy_flipster.user_data.ws_client import FlipsterUserWsClient

SYMBOL: str = "BTCUSDT.PERP"


def get_config() -> FlipsterApiConfig:
    api_key = os.environ.get("FLIPSTER_API_KEY", "")
    api_secret = os.environ.get("FLIPSTER_API_SECRET", "")
    if not api_key or not api_secret:
        print("ERROR: FLIPSTER_API_KEY, FLIPSTER_API_SECRET 환경변수 필요")
        sys.exit(1)
    return FlipsterApiConfig(api_key=api_key, api_secret=api_secret)


def quantize_down(value: Decimal, step: Decimal) -> Decimal:
    """value를 step 배수로 내림"""
    if step <= 0:
        return value
    return (value / step).to_integral_value(rounding=ROUND_DOWN) * step


def hr(title: str) -> None:
    print(f"\n{'=' * 6} {title} {'=' * (60 - len(title))}")


async def run_test(config: FlipsterApiConfig) -> int:
    exec_client = FlipsterExecutionClient(config)
    user_client = FlipsterUserRestClient(config)
    state = UserState()

    # WS 이벤트 기록
    ws_events: list[tuple[float, str]] = []

    def on_ws_update() -> None:
        ws_events.append((time.monotonic(), "update"))

    ws_client = FlipsterUserWsClient(config, state, on_update=on_ws_update)

    order_id: str | None = None
    exit_code = 0

    await exec_client.start()
    await user_client.start()
    ws_task = asyncio.create_task(ws_client.start())

    try:
        # ── 1. 초기 계정 스냅샷 ──
        hr("1. 초기 계정 스냅샷 (REST)")
        account_before = await user_client.get_account()
        print(f"  총 잔고:    {account_before.total_wallet_balance}")
        print(f"  미실현 PnL: {account_before.total_unrealized_pnl}")
        print(f"  가용 잔고:  {account_before.available_balance}")

        balances_before = await user_client.get_balances()
        print(f"  자산 {len(balances_before)}개")
        for b in balances_before:
            if b.balance != 0:
                print(f"    {b.asset}: balance={b.balance} avail={b.available_balance}")

        positions_before = await user_client.get_positions()
        open_pos = [p for p in positions_before if p.position_amount != 0]
        print(f"  포지션: {len(open_pos)}개 오픈 / 총 {len(positions_before)}")
        for p in open_pos[:5]:
            print(f"    {p.symbol} {p.position_side.value} qty={p.position_amount} pnl={p.unrealized_pnl}")

        # ── 2. 기존 대기 주문 확인 (전체 + 심볼별) ──
        hr(f"2. 기존 대기 주문 확인")
        all_pending = await exec_client.get_pending_orders()
        print(f"  전체: {len(all_pending)}개")
        for o in all_pending[:10]:
            print(f"    [{o.order_id[:8]}] {o.symbol} {o.side.value} {o.order_type.value} qty={o.quantity} price={o.price} status={o.status.value}")
        existing = [o for o in all_pending if o.symbol == SYMBOL]
        print(f"  {SYMBOL}: {len(existing)}개")

        # ── 3. 계약 스펙 + ticker ──
        hr(f"3. {SYMBOL} 스펙/시세 조회")
        contract = await exec_client.get_contract_info(SYMBOL)
        tick_size = Decimal(str(contract.get("tickSize", "0.1")))
        unit_qty = Decimal(str(contract.get("unitOrderQty", "0.001")))
        min_notional = Decimal(str(contract.get("notionalMinOrderAmount", "10")))
        print(f"  tickSize={tick_size} unitOrderQty={unit_qty} minNotional={min_notional}")

        ticker = await exec_client.get_ticker(SYMBOL)
        bid_price = Decimal(str(ticker.get("bidPrice", "0")))
        ask_price = Decimal(str(ticker.get("askPrice", "0")))
        last_price = Decimal(str(ticker.get("lastPrice", "0")))
        mid = (bid_price + ask_price) / 2 if bid_price > 0 and ask_price > 0 else last_price
        print(f"  bid={bid_price} ask={ask_price} last={last_price} mid={mid}")

        # LIMIT BUY @ mid * 0.9 (체결 불가)
        limit_price_raw = mid * Decimal("0.9")
        limit_price = quantize_down(limit_price_raw, tick_size)

        # 최소 notional 만족하는 수량
        qty_from_notional = (min_notional / limit_price) * Decimal("1.1")  # 10% 여유
        qty_raw = max(unit_qty, qty_from_notional)
        qty = quantize_down(qty_raw, unit_qty)
        if qty < unit_qty:
            qty = unit_qty
        # 다시 notional 체크
        while qty * limit_price < min_notional:
            qty += unit_qty

        notional = qty * limit_price
        print(f"  주문가: {limit_price} (mid×0.9)")
        print(f"  수량:   {qty} (notional={notional})")

        # 사용자 최종 확인 — 실계정이므로 안전장치
        print(f"\n  ⚠ 실계정에서 {SYMBOL} BUY LIMIT {qty} @ {limit_price} 제출 예정")
        print(f"     (mid 대비 -10% 가격이므로 정상 시장에서 체결 불가)")

        # ── 4. WS 연결 대기 + 이벤트 카운터 리셋 ──
        hr("4. WS 연결 대기")
        await asyncio.sleep(2.0)
        print(f"  초기 WS 이벤트: {len(ws_events)}개")
        print(f"  state.positions={len(state.positions)} state.balances={len(state.balances)} state.account={'있음' if state.account else '없음'}")
        ws_baseline = len(ws_events)

        # ── 4b. 트레이드 모드 + 심볼 초기 설정 ──
        hr("4b. 트레이드 모드 / 레버리지 설정")
        try:
            await exec_client.set_trade_mode("ONE_WAY")
            print("  ✓ tradeMode=ONE_WAY 설정 완료")
        except AppException as e:
            print(f"  ! set_trade_mode 실패(계속 진행): {e.error.message}")

        try:
            await exec_client.set_leverage(SYMBOL, 1, MarginType.CROSS)
            print(f"  ✓ {SYMBOL} leverage=1 margin=CROSS 설정 완료")
        except AppException as e:
            print(f"  ! set_leverage 실패(계속 진행): {e.error.message}")

        # ── 5. LIMIT 주문 제출 ──
        hr("5. LIMIT 주문 제출")
        req = OrderRequest(
            symbol=SYMBOL,
            side=OrderSide.BUY,
            order_type=OrderType.LIMIT,
            quantity=qty,
            price=limit_price,
            time_in_force=TimeInForce.GTC,
        )
        t0 = time.monotonic()
        resp = await exec_client.place_order(req)
        latency_ms = (time.monotonic() - t0) * 1000
        order_id = resp.order_id
        print(f"  orderId={order_id}")
        print(f"  status={resp.status.value}")
        print(f"  leaves={resp.leaves_qty}")
        print(f"  제출 레이턴시: {latency_ms:.1f} ms")

        # ── 6. REST 폴링 — pending에 나타나는지 ──
        hr("6. REST 폴링: 주문 가시성 확인")
        seen = False
        for i in range(10):
            pending = await exec_client.get_pending_orders(SYMBOL)
            found = next((o for o in pending if o.order_id == order_id), None)
            if found is not None:
                seen = True
                print(f"  ✓ 시도 {i+1}회: 발견 status={found.status.value} leaves={found.leaves_qty}")
                break
            await asyncio.sleep(0.3)
        if not seen:
            print(f"  ✗ 10회 시도 후에도 pending_orders에 없음")

        # ── 7. WS 이벤트 관찰 (2초) ──
        hr("7. WS 이벤트 관찰 (2초)")
        await asyncio.sleep(2.0)
        new_events = len(ws_events) - ws_baseline
        print(f"  주문 후 WS 이벤트: {new_events}개")
        if new_events > 0:
            deltas = [
                (ws_events[i][0] - t0) * 1000
                for i in range(ws_baseline, len(ws_events))
            ]
            print(f"  첫 이벤트까지: {deltas[0]:.1f} ms (주문 응답 시각 기준)")
            print(f"  이벤트 타이밍(ms): {[f'{d:.0f}' for d in deltas[:10]]}")
        print(f"  현재 state.positions={len(state.positions)} state.balances={len(state.balances)}")

        # ── 8. 주문 취소 ──
        hr("8. 주문 취소")
        cancel_t0 = time.monotonic()
        await exec_client.cancel_order(SYMBOL, order_id)
        cancel_latency = (time.monotonic() - cancel_t0) * 1000
        print(f"  취소 레이턴시: {cancel_latency:.1f} ms")

        # ── 9. REST 폴링 — 사라짐 확인 ──
        hr("9. REST 폴링: 취소 반영 확인")
        cancelled = False
        for i in range(10):
            pending = await exec_client.get_pending_orders(SYMBOL)
            if not any(o.order_id == order_id for o in pending):
                cancelled = True
                print(f"  ✓ 시도 {i+1}회: 취소 반영됨")
                break
            await asyncio.sleep(0.3)
        if not cancelled:
            print(f"  ✗ 10회 시도 후에도 pending에 남아있음 — 수동 확인 필요")
            exit_code = 1

        order_id = None  # 취소 완료 표시

        # ── 10. 최종 비교 ──
        hr("10. REST vs WS 최종 스냅샷 비교")
        account_after = await user_client.get_account()
        print(f"  REST 계정: wallet={account_after.total_wallet_balance} avail={account_after.available_balance}")
        if state.account is not None:
            print(f"  WS  계정:  wallet={state.account.total_wallet_balance} avail={state.account.available_balance}")
            match = (
                state.account.total_wallet_balance == account_after.total_wallet_balance
                and state.account.available_balance == account_after.available_balance
            )
            print(f"  일치 여부: {'✓' if match else '✗'}")
        else:
            print(f"  WS 계정 스냅샷 없음 (account 토픽 이벤트 미수신)")

        print(f"\n  총 WS 이벤트: {len(ws_events)}")
        print(f"  UserState: positions={len(state.positions)} balances={len(state.balances)}")

        hr("완료")
        print(f"  exit_code={exit_code}")

    except AppException as e:
        print(f"\n!!! AppException: {e.error.kind.value} - {e.error.message}")
        if e.error.status_code:
            print(f"    status_code={e.error.status_code}")
        exit_code = 2

    except Exception as e:
        print(f"\n!!! 예외: {type(e).__name__}: {e}")
        import traceback
        traceback.print_exc()
        exit_code = 3

    finally:
        # 안전장치: 미취소 주문 강제 취소
        if order_id is not None:
            print(f"\n[cleanup] 미취소 주문 {order_id} 강제 취소 시도")
            try:
                await exec_client.cancel_order(SYMBOL, order_id)
                print(f"  ✓ cleanup 성공")
            except Exception as e:
                print(f"  ✗ cleanup 실패: {e}")

        await ws_client.stop()
        try:
            await asyncio.wait_for(ws_task, timeout=2.0)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            ws_task.cancel()
        except Exception:
            pass

        await user_client.stop()
        await exec_client.stop()

    return exit_code


async def main() -> None:
    config = get_config()
    exit_code = await run_test(config)
    sys.exit(exit_code)


if __name__ == "__main__":
    asyncio.run(main())
