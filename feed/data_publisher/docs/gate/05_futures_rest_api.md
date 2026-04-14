# Gate.io Futures REST API v4 - Perpetual Contracts

> Base URL: `https://api.gateio.ws/api/v4`
> Settlement currencies (`{settle}`): `usdt` (linear), `btc` (inverse)

## Public Endpoints (No Auth Required)

---

### 1. List All Futures Contracts

```
GET /futures/{settle}/contracts
```

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| settle | string | Yes | - | `usdt` or `btc` |
| limit | integer | No | 100 | Max records (1-1000) |
| offset | integer | No | 0 | Starting position |

**Response: `Array<Contract>`**

| Field | Type | Description |
|-------|------|-------------|
| name | string | Contract name, e.g. `"BTC_USDT"` |
| type | string | `"direct"` (linear) or `"inverse"` |
| quanto_multiplier | string | Contract size multiplier |
| leverage_min | string | Min leverage |
| leverage_max | string | Max leverage |
| maintenance_rate | string | Maintenance margin rate |
| mark_type | string | `"index"` or `"internal"` |
| mark_price | string | Current mark price |
| index_price | string | Current index price |
| last_price | string | Last traded price |
| maker_fee_rate | string | Maker fee (negative = rebate) |
| taker_fee_rate | string | Taker fee |
| order_price_round | string | Min price tick |
| mark_price_round | string | Mark price precision |
| funding_rate | string | Current funding rate |
| funding_rate_indicative | string | Next period indicative funding rate |
| funding_next_apply | integer | Unix timestamp of next funding |
| funding_interval | integer | Funding interval in seconds (28800 = 8h) |
| funding_impact_value | string | Impact value for funding calculation |
| funding_offset | integer | Funding offset in seconds |
| funding_cap_ratio | string | Funding rate cap ratio |
| funding_rate_limit | string | Max funding rate |
| risk_limit_base | string | Base risk limit |
| risk_limit_step | string | Risk limit increment step |
| risk_limit_max | string | Max risk limit |
| order_size_min | integer | Min order size (contracts) |
| order_size_max | integer | Max order size (contracts) |
| order_price_deviate | string | Max deviation from mark price (ratio) |
| orders_limit | integer | Max open orders |
| trade_id | integer | Last trade ID |
| orderbook_id | integer | Current orderbook update ID |
| trade_size | integer | Total contracts traded historically |
| position_size | integer | Total open interest (contracts) |
| long_users | integer | Users with long positions |
| short_users | integer | Users with short positions |
| config_change_time | integer | Last config change timestamp |
| create_time | integer | Contract creation timestamp |
| launch_time | integer | Trading start timestamp |
| status | string | `"trading"`, `"pre_delisting"`, `"delisting"` |
| in_delisting | boolean | Being delisted? |
| ref_discount_rate | string | Referral discount rate |
| ref_rebate_rate | string | Referral rebate rate |
| cross_leverage_default | string | Default cross margin leverage |
| enable_bonus | boolean | Bonus enabled? |
| enable_credit | boolean | Credit enabled? |
| enable_decimal | boolean | Decimal order size enabled? |
| voucher_leverage | string | Voucher leverage |
| is_pre_market | boolean | Pre-market contract? |
| enable_circuit_breaker | boolean | Circuit breaker enabled? |
| market_order_slip_ratio | string | Max market order slippage |
| market_order_size_max | string | Max market order size |
| contract_type | string | `"stocks"` for stock-indexed, `""` for crypto |
| interest_rate | string | Interest rate for funding |

**Example:**
```
GET https://api.gateio.ws/api/v4/futures/usdt/contracts
```

---

### 2. Get a Single Contract

```
GET /futures/{settle}/contracts/{contract}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| settle | string | Yes | `usdt` or `btc` |
| contract | string | Yes | e.g. `BTC_USDT` |

**Response:** Single `Contract` object (same schema as above)

---

### 3. Futures Order Book

```
GET /futures/{settle}/order_book
```

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| settle | string | Yes | - | `usdt` or `btc` |
| contract | string | Yes | - | e.g. `BTC_USDT` |
| interval | string | No | `"0"` | Price aggregation (`"0"` = raw, `"0.1"`, `"1"`, `"10"`) |
| limit | integer | No | 10 | Depth levels (1-50 raw, 1-20 aggregated) |
| with_id | boolean | No | false | Include orderbook update ID |

**Response: `OrderBook`**

| Field | Type | Description |
|-------|------|-------------|
| id | integer | Update sequence ID (only with `with_id=true`) |
| current | float | Server timestamp (Unix with ms decimals) |
| update | float | Last update timestamp |
| asks | Array | Ask levels (price ascending) |
| asks[].p | string | Price |
| asks[].s | integer | Size (contracts) |
| bids | Array | Bid levels (price descending) |
| bids[].p | string | Price |
| bids[].s | integer | Size (contracts) |

**Example:**
```
GET https://api.gateio.ws/api/v4/futures/usdt/order_book?contract=BTC_USDT&limit=5&with_id=true
```

```json
{
  "id": 109796827144,
  "current": 1776162195.128,
  "update": 1776162195.127,
  "asks": [
    {"s": 32660, "p": "74528.9"},
    {"s": 1316, "p": "74529"}
  ],
  "bids": [
    {"s": 8596, "p": "74528.8"},
    {"s": 537, "p": "74527.9"}
  ]
}
```

---

### 4. Futures Trading History

```
GET /futures/{settle}/trades
```

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| settle | string | Yes | - | `usdt` or `btc` |
| contract | string | Yes | - | e.g. `BTC_USDT` |
| limit | integer | No | 100 | Max records (1-1000) |
| offset | integer | No | 0 | Starting position (deprecated) |
| last_id | integer | No | - | Return trades with ID < last_id |
| from | integer | No | - | Start timestamp (seconds) |
| to | integer | No | - | End timestamp (seconds) |

> `last_id` and `from`/`to` are mutually exclusive.

**Response: `Array<FuturesTrade>`**

| Field | Type | Description |
|-------|------|-------------|
| id | integer | Trade ID (globally unique, monotonically increasing) |
| contract | string | Contract name |
| create_time | float | Trade timestamp (Unix seconds with decimal) |
| create_time_ms | float | Same with ms precision |
| size | integer | Trade size. **Positive = buy/long taker, Negative = sell/short taker** |
| price | string | Trade price |

---

### 5. Futures Candlesticks

```
GET /futures/{settle}/candlesticks
```

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| settle | string | Yes | - | `usdt` or `btc` |
| contract | string | Yes | - | e.g. `BTC_USDT` |
| from | integer | No | - | Start timestamp |
| to | integer | No | - | End timestamp |
| limit | integer | No | 100 | Max points (max 2000) |
| interval | string | No | `"5m"` | `"10s"`, `"1m"`, `"5m"`, `"15m"`, `"30m"`, `"1h"`, `"4h"`, `"8h"`, `"1d"`, `"7d"` |
| timezone | string | No | `"utc0"` | `"all"`, `"utc0"`, `"utc8"` |

**Response: `Array<FuturesCandlestick>`**

---

### 6. Premium Index K-line

```
GET /futures/{settle}/premium_index
```

Same parameters as candlesticks. Max 1000 points per query.

---

### 7. List Futures Tickers

```
GET /futures/{settle}/tickers
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| settle | string | Yes | `usdt` or `btc` |
| contract | string | No | If omitted, returns ALL contracts |

**Response: `Array<FuturesTicker>`**

| Field | Type | Description |
|-------|------|-------------|
| contract | string | Contract name |
| last | string | Last traded price |
| low_24h | string | 24h lowest price |
| high_24h | string | 24h highest price |
| change_percentage | string | 24h price change % |
| change_price | string | Absolute 24h price change |
| volume_24h | string | 24h volume (contracts) |
| volume_24h_base | string | 24h volume (base currency) |
| volume_24h_quote | string | 24h volume (quote currency) |
| volume_24h_settle | string | 24h volume (settlement currency) |
| mark_price | string | Current mark price |
| index_price | string | Current index price |
| funding_rate | string | Current funding rate |
| funding_rate_indicative | string | Next period funding rate |
| **highest_bid** | string | **Best bid price** |
| **highest_size** | string | **Size at best bid (contracts)** |
| **lowest_ask** | string | **Best ask price** |
| **lowest_size** | string | **Size at best ask (contracts)** |
| total_size | string | Total open interest (contracts) |
| quanto_multiplier | string | Contract size multiplier |

**Example:**
```
GET https://api.gateio.ws/api/v4/futures/usdt/tickers?contract=BTC_USDT
```

```json
[{
  "contract": "BTC_USDT",
  "last": "74534.8",
  "low_24h": "70657.8",
  "high_24h": "74893.9",
  "change_percentage": "5.40",
  "volume_24h": "923939580",
  "mark_price": "74540.65",
  "index_price": "74579.01",
  "funding_rate": "0.000014",
  "highest_bid": "74535.5",
  "highest_size": "52278",
  "lowest_ask": "74535.6",
  "lowest_size": "112048",
  "total_size": "677127328",
  "quanto_multiplier": "0.0001"
}]
```

---

### 8. Funding Rate History

```
GET /futures/{settle}/funding_rate
```

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| settle | string | Yes | - | `usdt` or `btc` |
| contract | string | Yes | - | Contract name |
| limit | integer | No | 100 | Max records |
| from | integer | No | - | Start timestamp |
| to | integer | No | - | End timestamp |

---

### 9. Insurance Fund History

```
GET /futures/{settle}/insurance
```

---

### 10. Contract Stats

```
GET /futures/{settle}/contract_stats
```

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| settle | string | Yes | - | `usdt` or `btc` |
| contract | string | Yes | - | Contract name |
| from | integer | No | - | Start timestamp |
| interval | string | No | `"5m"` | Interval |
| limit | integer | No | 30 | Max records |

---

### 11. Index Constituents

```
GET /futures/{settle}/index_constituents/{index}
```

---

### 12. Liquidation Order History

```
GET /futures/{settle}/liq_orders
```

Max 3600s interval between from/to.

---

### 13. Risk Limit Tiers

```
GET /futures/{settle}/risk_limit_tiers
```

---

## Private Endpoints (Auth Required)

---

### 14. Futures Account

```
GET /futures/{settle}/accounts
```

---

### 15. Account Book

```
GET /futures/{settle}/account_book
```

| Change types: `dnw`, `pnl`, `fee`, `refr`, `fund`, etc.

---

### 16. List Positions

```
GET /futures/{settle}/positions
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| settle | string | Yes | Settlement currency |
| holding | boolean | No | Real positions only if true |
| limit | integer | No | Max records |
| offset | integer | No | Starting position |

---

### 17. Get Single Position

```
GET /futures/{settle}/positions/{contract}
```

---

### 18. Update Position Margin

```
POST /futures/{settle}/positions/{contract}/margin
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| change | string | Yes | Margin adjustment (positive = increase, negative = decrease) |

---

### 19. Update Position Leverage

```
POST /futures/{settle}/positions/{contract}/leverage
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| leverage | string | Yes | New leverage (0 = cross margin) |
| crossLeverageLimit | string | No | Cross margin leverage limit |

---

### 20. Switch Position Margin Mode

```
POST /futures/{settle}/positions/cross_mode
```

---

### 21. Switch Hedge Mode Cross/Isolated

```
POST /futures/{settle}/dual_comp/positions/cross_mode
```

---

### 22. Update Position Risk Limit

```
POST /futures/{settle}/positions/{contract}/risk_limit
```

---

### 23. Set Dual Mode (Hedge Mode)

```
POST /futures/{settle}/dual_mode
```

Requires all positions closed with no pending orders.

---

### 24. Get Dual Mode Position

```
GET /futures/{settle}/dual_comp/positions/{contract}
```

---

### 25-27. Dual Mode Position Operations

```
POST /futures/{settle}/dual_comp/positions/{contract}/margin
POST /futures/{settle}/dual_comp/positions/{contract}/leverage
POST /futures/{settle}/dual_comp/positions/{contract}/risk_limit
```

---

### 28. List Futures Orders

```
GET /futures/{settle}/orders
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| status | string | Yes | `"open"` |
| contract | string | No | Specific contract |
| limit | integer | No | Max 100 |
| offset | integer | No | Starting position |

---

### 29. Create Futures Order

```
POST /futures/{settle}/orders
```

**Order object fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| contract | string | Yes | Contract name |
| size | int64 | Yes | Positive = buy/long, Negative = sell/short |
| iceberg | string | No | Iceberg display size. 0 = non-iceberg |
| price | string | No | 0 = market order (tif must be `ioc`) |
| close | bool | No | true = close position |
| reduce_only | bool | No | Reduce-only order |
| tif | string | No | `"gtc"`, `"ioc"`, `"poc"`, `"fok"` |
| text | string | No | Custom order ID (max 28 chars, `t-` prefix) |
| auto_size | string | No | `"close_long"` or `"close_short"` for dual mode |
| stp_act | string | No | STP action: `"cn"`, `"co"`, `"cb"` |
| market_order_slip_ratio | string | No | Max slippage for market orders |

Header: `X-Gate-Exptime` (optional, expiration timestamp in ms)

---

### 30. Cancel All Open Orders

```
DELETE /futures/{settle}/orders
```

---

### 31. Query Orders by Time Range

```
GET /futures/{settle}/orders_timerange
```

---

### 32. Batch Create Orders

```
POST /futures/{settle}/batch_orders
```

Up to 10 orders per request.

---

### 33. Get Single Order

```
GET /futures/{settle}/orders/{order_id}
```

Supports custom text ID.

---

### 34. Amend Order

```
PUT /futures/{settle}/orders/{order_id}
```

Can amend size, price, and other parameters.

---

### 35. Cancel Single Order

```
DELETE /futures/{settle}/orders/{order_id}
```

---

### 36. Personal Trading History

```
GET /futures/{settle}/my_trades
```

---

### 37. Personal Trades by Time Range

```
GET /futures/{settle}/my_trades_timerange
```

---

### 38. Position Close History

```
GET /futures/{settle}/position_close
```

---

### 39. Liquidation History

```
GET /futures/{settle}/liquidates
```

---

### 40. Auto-Deleverage History

```
GET /futures/{settle}/auto_deleverages
```

---

### 41. Countdown Cancel All

```
POST /futures/{settle}/countdown_cancel_all
```

Dead man's switch. Cancels all orders after timeout.

---

### 42. Query Trading Fee

```
GET /futures/{settle}/fee
```

---

### 43. Batch Cancel Orders

```
POST /futures/{settle}/batch_cancel_orders
```

---

### 44. Batch Amend Orders

```
POST /futures/{settle}/batch_amend_orders
```

---

### 45. Risk Limit Table

```
GET /futures/{settle}/risk_limit_table
```

---

### 46-50. Price-Triggered (Auto) Orders

```
GET    /futures/{settle}/price_orders          # List auto orders
POST   /futures/{settle}/price_orders          # Create auto order
DELETE /futures/{settle}/price_orders          # Cancel all auto orders
GET    /futures/{settle}/price_orders/{order_id}  # Get single auto order
DELETE /futures/{settle}/price_orders/{order_id}  # Cancel single auto order
```

## Response Headers (All Endpoints)

| Header | Description |
|--------|-------------|
| `X-Gate-Ratelimit-Limit` | Max requests per window (200) |
| `X-Gate-Ratelimit-Requests-Remain` | Remaining requests |
| `X-Gate-Ratelimit-Reset-Timestamp` | Window reset timestamp |
| `X-Gate-Trace-Id` | Request trace ID |
| `X-Request-Id` | Request ID |
| `X-In-Time` | Request received (microseconds) |
| `X-Out-Time` | Response sent (microseconds) |

## Funding Rate Notes

- Applied every `funding_interval` seconds (typically 28800 = 8 hours)
- `funding_next_apply`: Unix timestamp of next funding event
- `funding_rate`: Current period rate
- `funding_rate_indicative`: Estimated next period rate
- Capped by `funding_rate_limit` and `funding_cap_ratio`
