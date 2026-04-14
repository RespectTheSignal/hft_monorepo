# Gate.io API v4 Rate Limits

## REST API Rate Limits

### Public Endpoints

| Endpoint Type | Rate Limit | Based On |
|---------------|-----------|----------|
| All public endpoints | 200 req / 10s | IP |

### Private Endpoints by Market

| Market | Operation | Rate Limit | Based On |
|--------|-----------|-----------|----------|
| Wallet | Withdrawal | 1 req / 3s | UID |
| Wallet | UID transfer | 1 req / 10s | UID |
| Wallet | Transfer between accounts | 80 req / 10s | UID |
| Wallet | Others | 200 req / 10s | UID |
| Spot | Order placement/modification | 10 req / s (total) | UID + Market |
| Spot | Order cancellation | 200 req / s (total) | UID |
| Spot | Others | 200 req / 10s | UID |
| Perpetual Futures | Order placement/modification | 100 req / s (total) | UID |
| Perpetual Futures | Order cancellation | 200 req / s | UID |
| Perpetual Futures | Others | 200 req / 10s | UID |
| Delivery | Order placement | 500 req / 10s | UID |
| Delivery | Order cancellation | 500 req / 10s | UID |
| Delivery | Others | 200 req / 10s | UID |
| Options | Order placement | 200 req / s | UID |
| Options | Order cancellation | 200 req / s | UID |
| Options | Others | 200 req / 10s | UID |
| Subaccount | Per endpoint | 80 req / 10s | UID |
| Unified | Borrow/repay | 15 req / 10s | UID |
| Other Private | Per endpoint | 150 req / 10s | UID |

### Alternative Rate Limit Numbers (from GitHub README)

| Market | Type | Rate Limit |
|--------|------|-----------|
| Spot | Public endpoints | 900 req / s (API Key based) |
| Spot | Private endpoints | 900 req / s (API Key based) |
| Spot | Order cancellations | 5000 req / s (API Key based) |
| Perpetual Swaps | Public endpoints | 300 req / s (IP based) |
| Perpetual Swaps | Private/trading | 400 req / s (User ID based) |

> Note: Rate limit values may differ between documentation sources. Check `X-Gate-RateLimit-*` response headers for authoritative values.

### Rate Limit Response Headers

| Header | Description |
|--------|-------------|
| `X-Gate-RateLimit-Requests-Remain` | Remaining requests for current endpoint |
| `X-Gate-RateLimit-Limit` | Current limit for current endpoint |
| `X-Gate-RateLimit-Reset-Timestamp` | Unix timestamp when the window resets |

### Futures Tier-Based Rate Limits (VIP 14+, based on fill ratio)

| Tier | Fill Ratio | Rate Limit |
|------|-----------|-----------|
| Tier 1 | [0, 1) | 100 req / s |
| Tier 2 | [1, 3) | 150 req / s |
| Tier 3 | [3, 5) | 200 req / s |
| Tier 4 | [5, 10) | 250 req / s |
| Tier 5 | [10, 20) | 300 req / s |
| Tier 6 | [20, 50) | 350 req / s |
| Tier 7 | >= 50 | 400 req / s |

### Fill Ratio Calculation

Fill Ratio = (Trading Volume Weight / Order Cancellation Volume Weight) * 100%

Where:
- Trading Volume Weight = Sum(trade_amount * symbol_multiplier)
- Order Cancellation Volume Weight = Sum(cancel_amount * symbol_multiplier)

## WebSocket Rate Limits

| Market | Operation | Rate Limit |
|--------|-----------|-----------|
| Spot | Order placement/modification | 10 req / s |
| Futures | Order operations | 100 req / s |
| Others | No limit | - |
| Max connections per IP | All | 300 |

### WebSocket API (Spot Account Trade) Error Codes

| Code | Description |
|------|-------------|
| 100 | Internal rate limiting exception error |
| 211 | Spot trading rate limit |
| 212 | Spot rate limiting based on fill rate |

### WebSocket Trading API Rate Limit Headers

Response `header` includes:
- `x_gate_ratelimit_requests_remain`: Remaining available requests in current time window (hidden if 0)
- `x_gate_ratelimit_limit`: Current rate limit cap (hidden if 0)
- `x_gat_ratelimit_reset_timestamp`: Timestamp (ms) when the rate limit resets
