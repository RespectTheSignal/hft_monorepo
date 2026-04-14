# Gate.io API v4 Overview

> Version: v4.106.58
> Spec: OpenAPI Specification 3.0.2
> Data Center: AWS Japan (ap-northeast-1)

## Base URLs

| Environment | URL | Notes |
|-------------|-----|-------|
| Live (REST) | `https://api.gateio.ws/api/v4` | Primary |
| Live (Futures REST alt) | `https://fx-api.gateio.ws/api/v4` | Futures only |
| TestNet (REST) | `https://fx-api-testnet.gateio.ws/api/v4` | Futures TestNet |
| TestNet (REST alt) | `https://api-testnet.gateapi.io/api/v4` | |
| Futures WS USDT (Live) | `wss://fx-ws.gateio.ws/v4/ws/usdt` | |
| Futures WS BTC (Live) | `wss://fx-ws.gateio.ws/v4/ws/btc` | |
| Futures WS USDT (TestNet) | `wss://ws-testnet.gate.com/v4/ws/futures/usdt` | |
| Futures WS BTC (TestNet) | `wss://fx-ws-testnet.gateio.ws/v4/ws/btc` | |
| Spot WS (Live) | `wss://api.gateio.ws/ws/v4/` | |
| Spot WS (TestNet) | `wss://ws-testnet.gate.com/v4/ws/spot` | |

> **WARNING:** Legacy Futures WS URLs (`wss://fx-ws.gateio.ws/v4/ws`, `wss://fx-ws-testnet.gateio.ws/v4/ws`) are deprecated and will be redirected.

## API Categories

| URL Prefix | Category | Description |
|---|---|---|
| `/api/v4/spot/*` | Spot Trading | Currency status, market info, orders, trade records |
| `/api/v4/margin/*` | Margin Trading | Margin account management, lending, repayment |
| `/api/v4/wallet/*` | Wallet Management | Deposit/withdrawal records, balance queries, fund transfers |
| `/api/v4/withdrawals/*` | Withdrawal | Withdrawal of digital currency |
| `/api/v4/futures/{settle}/*` | Perpetual Futures | USDT-settled and BTC-settled perpetual contracts |
| `/api/v4/delivery/{settle}/*` | Delivery Futures | Delivery/expiring futures contracts |
| `/api/v4/options/*` | Options | Options trading |
| `/api/v4/unified/*` | Unified Account | Unified account operations |
| `/api/v4/earn/*` | Earn | Lend & Earn operations |
| `/api/v4/loan/*` | Collateral Loan | Collateral/multi-collateral loans |
| `/api/v4/flash_swap/*` | Flash Swap | Flash swap operations |
| `/api/v4/sub_accounts/*` | SubAccount | Sub-account management |
| `/api/v4/rebate/*` | Rebate | Rebate/commission data |
| `/api/v4/account/*` | Account | Account details |

## SDKs

| Language | Repository |
|----------|------------|
| Python | `github.com/gateio/gateapi-python` |
| Java | `github.com/gateio/gateapi-java` |
| PHP | `github.com/gateio/gateapi-php` |
| Go | `github.com/gateio/gateapi-go` |
| C# | `github.com/gateio/gateapi-csharp` |
| NodeJS | `github.com/gateio/gateapi-nodejs` |
| Javascript | `github.com/gateio/gateapi-js` |

WebSocket SDK: `github.com/gateio/gatews`

Python, Java, C#, Go SDKs include demo applications.

## General Conventions

### HTTP Convention

- All responses are JSON format
- Time format: Unix timestamp in seconds (may be int64, number, or string with decimal places)
- Response headers include:
  - `X-In-Time` / `X-Out-Time`: microsecond timestamps for gateway in/out
  - `X-Gate-Trace-ID`: request trace ID for tracking
  - `X-Request-Id`: request ID

### Data Types

| Type | Description |
|------|-------------|
| `string` | Includes price and amount values |
| `integer` | 32-bit, for status codes, size, times |
| `integer(int64)` | 64-bit, for IDs and high-precision timestamps |
| `float` | For some time and stat fields |
| `object` | JSON object |
| `array` | JSON array |
| `boolean` | true/false |

### Pagination

Two methods supported:

1. **page-limit**: `page` starts from 1
2. **limit-offset**: `offset` starts from 0

Defaults: `limit=100`, max `limit=1000`

Response headers:
- `X-Pagination-Limit`
- `X-Pagination-Offset`
- `X-Pagination-Total`

### Return Format

All responses use JSON. Error responses:

```json
{
  "label": "ERROR_LABEL",
  "message": "Human readable error description"
}
```

### HTTP Status Codes

| Code | Description |
|------|-------------|
| 200/201 | Request succeeded |
| 202 | Accepted, processing not done yet |
| 204 | Succeeded, no body |
| 400 | Invalid request |
| 401 | Authentication failed |
| 404 | Not found |
| 429 | Too many requests |
| 5xx | Server error |

## Matching Mechanism

### Matching Priority
Price priority, then time priority.

### Order Life Cycle
Orders go through: new -> open -> (partially filled) -> filled/cancelled

## Self-Trade Prevention (STP)

Three strategies:
- **CN** (Cancel New): Cancel the new order
- **CO** (Cancel Old): Cancel the existing order
- **CB** (Cancel Both): Cancel both orders

Use `stp_act` parameter when placing orders. Default is `cn`.
Users must be in the same STP trading group (`stp_id`).

## Unified Account Modes

| Mode | Description |
|------|-------------|
| `classic` | Classic account mode |
| `multi_currency` | Cross-currency margin mode |
| `portfolio` | Portfolio margin mode |
| `single_currency` | Single-currency margin mode |

## Market Maker Program

Contact: mm@mail.gate.io
- Provide Gate.io UID and trading volume documentation
- Approval within 3 business days
- VIP11+ members must enable GT deductions for professional rates

## Contract Size Calculation

### USDT-settled (linear/direct)
- 1 contract = `quanto_multiplier` units of the base asset
- BTC_USDT: `quanto_multiplier = "0.0001"` -> 1 contract = 0.0001 BTC
- Trade value in USDT = contracts * quanto_multiplier * price

### BTC-settled (inverse)
- 1 contract = fixed USD face value
- Contracts denominated in USD, settled in BTC
