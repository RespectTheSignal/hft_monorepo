# Flipster API Documentation

> Source: https://api-docs.flipster.io/
> Production Host: `https://trading-api.flipster.io`
> Base Path: `/api/v1/`

## 문서 구조

| File | Description |
|------|-------------|
| [00_overview.md](00_overview.md) | Host, 거래 모드, API 카테고리, Rate Limiting |
| [01_authentication.md](01_authentication.md) | HMAC-SHA256 인증 가이드 (Python/JS 코드 포함) |
| [02_essential_concepts.md](02_essential_concepts.md) | 필수 개념 (One-Way 모드, 레버리지, 주문 유형 등) |
| [03_websocket.md](03_websocket.md) | WebSocket 연결, 인증, 토픽, 메시지 형식 |
| [04_trade_api.md](04_trade_api.md) | Trade 엔드포인트 (9개) |
| [05_market_api.md](05_market_api.md) | Market 엔드포인트 (6개) |
| [06_account_api.md](06_account_api.md) | Account 엔드포인트 (5개) |
| [07_public_api.md](07_public_api.md) | Public 엔드포인트 (2개) |
| [08_affiliate_api.md](08_affiliate_api.md) | Affiliate 엔드포인트 (2개) |

## API Endpoint Quick Reference

### Trade (`/api/v1/trade/`)
| Method | Path | Description |
|--------|------|-------------|
| POST | `/trade/order` | Place Order |
| PUT | `/trade/order` | Set TP/SL |
| GET | `/trade/order` | Get Pending Orders |
| DELETE | `/trade/order` | Cancel Pending Order |
| POST | `/trade/leverage` | Set Leverage/Margin Mode |
| POST | `/trade/margin` | Adjust Margin |
| GET | `/trade/symbol` | Get Tradable Symbols |
| GET | `/trade/order/history` | Get Order History |
| GET | `/trade/execution/history` | Get Execution History |

### Market (`/api/v1/market/`)
| Method | Path | Description |
|--------|------|-------------|
| GET | `/market/contract` | Get Contract Info |
| GET | `/market/ticker` | Get Tickers (BookTicker: bidPrice, askPrice) |
| GET | `/market/funding-info` | Get Funding Info |
| GET | `/market/kline` | Get Kline (Candlesticks) |
| GET | `/market/orderbook` | Get Orderbook |
| GET | `/market/fee-rate` | Get Trading Fee Rate |

### Account (`/api/v1/account/`)
| Method | Path | Description |
|--------|------|-------------|
| GET | `/account` | Get Account Info |
| GET | `/account/margin` | Get Margin Info |
| GET | `/account/balance` | Get Account Balance |
| GET | `/account/position` | Get Position Info |
| PUT | `/account/trade-mode` | Set Trade Mode |

### Public (`/api/v1/public/`)
| Method | Path | Description |
|--------|------|-------------|
| GET | `/public/time` | Get Server Time |
| GET | `/public/ping` | Check Service Liveliness |

### Affiliate (`/api/v1/affiliate/`)
| Method | Path | Description |
|--------|------|-------------|
| GET | `/affiliate/overview-referral-history-csv` | Download Referee History (CSV) |
| GET | `/affiliate/user-info` | Get User Info |

### WebSocket
| Endpoint | Topics |
|----------|--------|
| `wss://trading-api.flipster.io/api/v1/stream` | `ticker.{symbol}`, `kline.{interval}.{symbol}`, `orderbook.{symbol}`, `account`, `account.margin`, `account.balance`, `account.position` |
