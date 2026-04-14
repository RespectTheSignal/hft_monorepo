# Gate.io API v4 Documentation

> API Version: v4.106.58 (latest as of 2026-04)
> Last updated: 2026-04-14

HFT 트레이딩을 위한 Gate.io API v4 전체 문서 정리.
Contract bookticker 수신, QuestDB 저장, ZMQ 배포를 위한 참고 문서.

## Index

| # | File | Description |
|---|------|-------------|
| 1 | [01_overview.md](01_overview.md) | API 개요, Base URL, SDK, 데이터 센터 |
| 2 | [02_authentication.md](02_authentication.md) | 인증 (HMAC-SHA512 서명), API Key 관리 |
| 3 | [03_rate_limits.md](03_rate_limits.md) | Rate Limit 규칙 (REST / WebSocket) |
| 4 | [04_error_codes.md](04_error_codes.md) | 에러 코드 및 라벨 전체 목록 |
| 5 | [05_futures_rest_api.md](05_futures_rest_api.md) | Futures REST API 엔드포인트 (50개) |
| 6 | [06_futures_websocket.md](06_futures_websocket.md) | Futures WebSocket v4 채널 (30개) |
| 7 | [07_spot_websocket.md](07_spot_websocket.md) | Spot WebSocket v4 채널 (27개) |
| 8 | [08_rest_api_categories.md](08_rest_api_categories.md) | REST API 전체 카테고리 및 엔드포인트 목록 |

## Quick Reference

### Base URLs

| Purpose | URL |
|---------|-----|
| REST API (Live) | `https://api.gateio.ws/api/v4` |
| REST API (Futures Alt) | `https://fx-api.gateio.ws/api/v4` |
| REST API (TestNet) | `https://fx-api-testnet.gateio.ws/api/v4` |
| Futures WS (USDT) | `wss://fx-ws.gateio.ws/v4/ws/usdt` |
| Futures WS (BTC) | `wss://fx-ws.gateio.ws/v4/ws/btc` |
| Spot WS | `wss://api.gateio.ws/ws/v4/` |

### Key Channels for HFT

| Channel | Description | Update Speed |
|---------|-------------|--------------|
| `futures.book_ticker` | Best bid/ask (BBO) | realtime (~10ms) |
| `futures.order_book_update` | Incremental orderbook | 20ms / 100ms / 1000ms |
| `futures.obu` | Order book v2 (fastest) | realtime |
| `futures.trades` | Public trades | realtime |
| `futures.tickers` | Ticker overview | ~1s |
| `spot.book_ticker` | Spot best bid/ask | realtime |
