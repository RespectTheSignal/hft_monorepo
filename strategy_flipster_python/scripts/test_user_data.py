"""Flipster 사용자 데이터 테스트 — REST + WS.

환경변수 필요:
    FLIPSTER_API_KEY, FLIPSTER_API_SECRET

사용법:
    # REST만 테스트
    python scripts/test_user_data.py rest

    # WS 스트림 테스트
    python scripts/test_user_data.py ws

    # 둘 다
    python scripts/test_user_data.py
"""

from __future__ import annotations

import asyncio
import os
import sys

sys.path.insert(0, "src")

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.user_data.rest_client import FlipsterUserRestClient
from strategy_flipster.user_data.state import UserState
from strategy_flipster.user_data.ws_client import FlipsterUserWsClient


def get_config() -> FlipsterApiConfig:
    api_key = os.environ.get("FLIPSTER_API_KEY", "")
    api_secret = os.environ.get("FLIPSTER_API_SECRET", "")
    if not api_key or not api_secret:
        print("ERROR: FLIPSTER_API_KEY, FLIPSTER_API_SECRET 환경변수 필요")
        sys.exit(1)
    return FlipsterApiConfig(api_key=api_key, api_secret=api_secret)


async def test_rest(config: FlipsterApiConfig) -> None:
    print("=== REST API 테스트 ===")
    client = FlipsterUserRestClient(config)
    await client.start()

    try:
        # 계정 정보
        account = await client.get_account()
        print(f"\n[Account]")
        print(f"  총 잔고:      {account.total_wallet_balance}")
        print(f"  미실현 PnL:   {account.total_unrealized_pnl}")
        print(f"  마진 잔고:    {account.total_margin_balance}")
        print(f"  가용 잔고:    {account.available_balance}")

        # 잔고
        balances = await client.get_balances()
        print(f"\n[Balances] ({len(balances)}개)")
        for b in balances:
            print(f"  {b.asset:>6}: balance={b.balance:>15} available={b.available_balance:>15}")

        # 포지션
        positions = await client.get_positions()
        print(f"\n[Positions] ({len(positions)}개)")
        for p in positions:
            if p.position_amount != 0:
                print(
                    f"  {p.symbol:<20} {p.position_side.value:>5} "
                    f"qty={p.position_amount:>12} entry={p.entry_price:>12} "
                    f"mark={p.mark_price:>12} pnl={p.unrealized_pnl:>10} "
                    f"lev={p.leverage}x {p.margin_type.value}"
                )

    finally:
        await client.stop()


async def test_ws(config: FlipsterApiConfig) -> None:
    print("\n=== WS 스트림 테스트 (Ctrl+C로 종료) ===")
    state = UserState()
    update_count = 0

    def on_update() -> None:
        nonlocal update_count
        update_count += 1
        if update_count <= 10 or update_count % 100 == 0:
            print(f"\n[Update #{update_count}]")
            if state.account:
                print(f"  Account: balance={state.account.total_wallet_balance} pnl={state.account.total_unrealized_pnl}")
            print(f"  Positions: {len(state.positions)}개")
            for sym, pos in list(state.positions.items())[:5]:
                if pos.position_amount != 0:
                    print(f"    {sym}: {pos.position_side.value} qty={pos.position_amount} pnl={pos.unrealized_pnl}")
            print(f"  Balances: {len(state.balances)}개")

    ws_client = FlipsterUserWsClient(config, state, on_update=on_update)

    try:
        await ws_client.start()
    except KeyboardInterrupt:
        pass
    finally:
        await ws_client.stop()
        print(f"\n총 {update_count}개 업데이트 수신")


async def main() -> None:
    config = get_config()
    mode = sys.argv[1] if len(sys.argv) > 1 else "all"

    if mode in ("rest", "all"):
        await test_rest(config)
    if mode in ("ws", "all"):
        await test_ws(config)


if __name__ == "__main__":
    asyncio.run(main())
