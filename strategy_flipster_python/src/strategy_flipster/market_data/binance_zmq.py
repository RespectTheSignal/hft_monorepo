"""Binance ZMQ SUB 클라이언트 — ExchangeZmqFeed wrapper (하위 호환)"""

from __future__ import annotations

from strategy_flipster.market_data.zmq_feed import ExchangeZmqFeed

# 하위 호환: BinanceZmqFeed = ExchangeZmqFeed
BinanceZmqFeed = ExchangeZmqFeed
