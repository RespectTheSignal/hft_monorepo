# Gate.io Futures WebSocket v4

> USDT: `wss://fx-ws.gateio.ws/v4/ws/usdt`
> BTC: `wss://fx-ws.gateio.ws/v4/ws/btc`
> TestNet USDT: `wss://ws-testnet.gate.com/v4/ws/futures/usdt`
> TestNet BTC: `wss://fx-ws-testnet.gateio.ws/v4/ws/btc`

## Message Format

### Request

```json
{
  "id": 123456,
  "time": 1611541000,
  "channel": "futures.book_ticker",
  "event": "subscribe",
  "payload": ["BTC_USDT"],
  "auth": {
    "method": "api_key",
    "KEY": "xxx",
    "SIGN": "xxx"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| id | Integer | No | Request ID (echoed back in response) |
| time | Integer | Yes | Request time (Unix seconds) |
| channel | String | Yes | Channel name |
| auth | Object | No | Auth for private channels |
| event | String | Yes | `"subscribe"` / `"unsubscribe"` / `"api"` |
| payload | Array | Yes | Channel-specific parameters |

### Response

```json
{
  "time": 1611541000,
  "time_ms": 1611541000001,
  "channel": "futures.book_ticker",
  "event": "update",
  "error": null,
  "result": { ... }
}
```

| Field | Type | Description |
|-------|------|-------------|
| time | Integer | Response time (seconds) |
| time_ms | Integer | Response time (milliseconds) |
| channel | String | Channel name |
| event | String | `"update"` / `"all"` |
| error | Object | null on success |
| result | Any | Channel-specific data |

### Error Codes

| Code | Message |
|------|---------|
| 1 | invalid argument struct |
| 2 | invalid argument |
| 3 | service error |
| 4 | authentication fail |

## System API

### Ping/Pong

```json
{"time": 123456, "channel": "futures.ping"}
// Response:
{"channel": "futures.pong", ...}
```

Server initiates protocol-level ping. Client must reply or gets disconnected.
Application-level `futures.ping` resets the timeout timer.

### Service Upgrade Notification

```json
{
  "type": "service_upgrade",
  "msg": "The connection will soon be closed for a service upgrade. Please reconnect."
}
```

---

## Public Channels

---

### 1. futures.tickers

Ticker overview: highest, lowest, last trade price, daily volume, price change.

**Subscribe:**
```json
{"time":123456, "channel":"futures.tickers", "event":"subscribe", "payload":["BTC_USDT"]}
```

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| contract | String | Contract name |
| last | String | Last price |
| change_percentage | String | 24h change % |
| funding_rate | String | Funding rate |
| funding_rate_indicative | String | Next period indicative rate (deprecated) |
| mark_price | String | Mark price |
| index_price | String | Index price |
| total_size | String | Total open interest |
| volume_24h | String | 24h volume (contracts) |
| volume_24h_quote | String | 24h volume (quote currency) |
| volume_24h_settle | String | 24h volume (settle currency) |
| volume_24h_base | String | 24h volume (base currency) |
| low_24h | String | 24h lowest |
| high_24h | String | 24h highest |
| quanto_base_rate | String | Quanto exchange rate (Quanto contracts only) |

---

### 2. futures.trades

Trade messages in realtime.

**Subscribe:**
```json
{"time":123456, "channel":"futures.trades", "event":"subscribe", "payload":["BTC_USDT"]}
```

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| contract | String | Contract name |
| size | String/Int | Trade size. **Positive = buyer taker, Negative = seller taker** |
| id | Integer | Trade ID |
| create_time | Integer | Timestamp (seconds) |
| create_time_ms | Integer | Timestamp (milliseconds) |
| price | String | Trade price |
| is_internal | Boolean | Internal trade (insurance fund/ADL takeover). Not in normal trades |

---

### 3. futures.book_ticker ★ (Best Bid/Ask - BBO)

**Realtime best bid and ask. Primary channel for HFT bookticker.**

**Subscribe:**
```json
{"time":123456, "channel":"futures.book_ticker", "event":"subscribe", "payload":["BTC_USDT"]}
```

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| **t** | Integer | Timestamp (milliseconds) |
| **u** | String | Order book update ID |
| **s** | String | Contract name |
| **b** | String | Best bid price (empty string if no bids) |
| **B** | String/Integer | Best bid size (0 if no bids) |
| **a** | String | Best ask price (empty string if no asks) |
| **A** | String/Integer | Best ask size (0 if no asks) |

**Example notification:**
```json
{
  "time": 1234567890,
  "time_ms": 1234567890123,
  "channel": "futures.book_ticker",
  "event": "update",
  "result": {
    "t": 1234567890123,
    "u": "109796827144",
    "s": "BTC_USDT",
    "b": "74535.5",
    "B": "52278",
    "a": "74535.6",
    "A": "112048"
  }
}
```

**Unsubscribe:**
```json
{"time":123456, "channel":"futures.book_ticker", "event":"unsubscribe", "payload":["BTC_USDT"]}
```

---

### 4. futures.order_book (Legacy - Full Snapshots)

> **Not recommended. Use `futures.order_book_update` or `futures.obu` instead.**

**Subscribe:**
```json
{"time":123456, "channel":"futures.order_book", "event":"subscribe", "payload":["BTC_USDT","20","0"]}
```

| Param | Description |
|-------|-------------|
| contract | Contract name |
| limit | Depth: `"1"`, `"5"`, `"10"`, `"20"`, `"50"`, `"100"` |
| interval | `"0"` |

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | Integer | Timestamp (ms) |
| contract | String | Contract name |
| id | Integer | OrderBook ID |
| asks | Array | Ask list `[{p, s}, ...]` |
| bids | Array | Bid list `[{p, s}, ...]` |
| level | String | Depth level |

---

### 5. futures.order_book_update (Incremental Updates)

**Subscribe:**
```json
{"time":123456, "channel":"futures.order_book_update", "event":"subscribe", "payload":["BTC_USDT","100ms","100"]}
```

| Param | Description |
|-------|-------------|
| contract | Contract name |
| frequency | `"20ms"`, `"100ms"`, `"1000ms"` |
| level | Optional: `"20"`, `"50"`, `"100"` |

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | Integer | Timestamp (ms) |
| e | Boolean | `true` = full depth snapshot |
| s | String | Contract name |
| U | Integer | First update ID since last |
| u | Integer | Last update ID since last |
| b | Array | Changed bids `[{p, s}, ...]` |
| a | Array | Changed asks `[{p, s}, ...]` |
| level | String | Depth level |

> `s` = 0 means remove this price level from the book.

**Maintaining local order book:**
1. Subscribe to `futures.order_book_update` with desired level
2. Get initial snapshot via REST: `GET /futures/usdt/order_book?contract=BTC_USDT&limit=10&with_id=true`
3. Apply incremental updates, matching by update ID sequence

---

### 6. futures.obu (Order Book V2 - Fastest)

**Subscribe:**
```json
{"time":123456, "channel":"futures.obu", "event":"subscribe", "payload":["ob.BTC_USDT.400"]}
```

Format: `ob.<CONTRACT>.<LEVELS>` (e.g., `ob.BTC_USDT.400`)

**Behavior:**
- First message after subscribe is a **full depth snapshot** (`e: true`)
- Subsequent messages are incremental
- **One subscription per contract per connection** (duplicates error)
- If data is not continuous, must unsubscribe/resubscribe

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | Integer | Timestamp (ms) |
| e | Boolean | `true` = full snapshot |
| s | String | Depth stream name |
| U | Integer | Starting update ID |
| u | Integer | Ending update ID |
| b | Array | Bids `[[price, amount], ...]` |
| a | Array | Asks `[[price, amount], ...]` |

> Amount = 0 means remove entry from local depth.

---

### 7. futures.candlesticks

**Subscribe:**
```json
{"time":123456, "channel":"futures.candlesticks", "event":"subscribe", "payload":["1m","BTC_USDT"]}
```

Intervals: `"10s"`, `"1m"`, `"5m"`, `"15m"`, `"30m"`, `"1h"`, `"4h"`, `"8h"`, `"1d"`, `"7d"`

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | Integer | Timestamp |
| o | String | Open |
| c | String | Close |
| h | String | High |
| l | String | Low |
| v | String | Volume |
| n | String | Amount |

---

### 8. futures.public_liquidates

Public liquidation orders. Each contract pushes up to 1 liquidation per second.

**Subscribe:**
```json
{"time":123456, "channel":"futures.public_liquidates", "event":"subscribe", "payload":["BTC_USDT"]}
```

---

### 9. futures.contract_stats

**Subscribe:**
```json
{"time":123456, "channel":"futures.contract_stats", "event":"subscribe", "payload":["BTC_USDT","1m"]}
```

Intervals: `"1m"`, `"5m"`, `"15m"`, `"30m"`, `"1h"`, `"4h"`, `"8h"`, `"1d"`, `"3d"`, `"7d"`

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| contract | String | Contract name |
| stat_time | Integer | Stat timestamp |
| lsr_taker | String | Long/short taker size ratio |
| lsr_account | String | Top long/short account ratio |
| long_liq_size | String | Long liquidation size |
| short_liq_size | String | Short liquidation size |
| open_interest | String | Open interest |
| open_interest_usd | String | Open interest (quote currency) |
| top_lsr_account | String | Top long/short account ratio |
| top_lsr_size | String | Top long/short size ratio |

---

## Private Channels (Auth Required)

---

### 10. futures.orders

**Subscribe:**
```json
{
  "time":123456,
  "channel":"futures.orders",
  "event":"subscribe",
  "payload":["BTC_USDT"],
  "auth": {"method":"api_key", "KEY":"xxx", "SIGN":"xxx"}
}
```

Use `["!all"]` to subscribe to all contracts.

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| contract | String | Contract name |
| id | Integer | Order ID |
| size | String/Integer | Order size (positive=bid, negative=ask) |
| iceberg | String | Iceberg display size (0 = non-iceberg) |
| left | String/Integer | Size remaining |
| price | String | Order price (0 = market order) |
| status | String | Order status |
| mkfr | String | Maker fee |
| tkfr | String | Taker fee |
| refu | Integer | Reference user ID |
| tif | String | Time in force |
| create_time | Integer | Creation timestamp (ms) |
| finish_time | Integer | Finished timestamp (seconds, 0 if open) |
| finish_time_ms | Integer | Finished timestamp (ms) |
| is_close | Boolean | Close order? |
| is_reduce_only | Boolean | Reduce only? |
| is_liq | Boolean | Liquidation order? |
| text | String | User-defined text |
| stp_id | Integer | STP group ID |
| stp_act | String | STP action |
| auto_size | String | Auto size for dual-mode close |
| fill_price | String | Fill price |
| finish_as | String | How order was finished |
| market_order_slip_ratio | String | Preset slippage ratio |

---

### 11. futures.usertrades

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| contract | String | Contract name |
| size | String/Integer | Trade size |
| id | Integer | Trade ID |
| create_time | Integer | Timestamp |
| create_time_ms | Integer | Timestamp (ms) |
| price | String | Trade price |
| order_id | String | Order ID |
| role | String | `"maker"` / `"taker"` |
| text | String | User-defined text |
| fee | String | Fee |
| point_fee | String | Points used to deduct fee |

---

### 12. futures.liquidates (Private)

User liquidation events.

---

### 13. futures.auto_deleverages

ADL events. Fields: `contract`, `position_size`, `trade_size`, etc.

---

### 14. futures.position_closes

Position close events. Fields: `pnl`, `pnl_pnl`, `pnl_fund`, `pnl_fee`, `text`, `close_time`, etc.

---

### 15. futures.balances

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| balance | String | Balance after change |
| change | String | Change amount |
| fund_type | String | Change type |
| time_ms | Integer | Timestamp (ms) |

---

### 16. futures.reduce_risk_limits

Risk limit reduction events.

---

### 17. futures.positions

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| contract | String | Contract name |
| size | String/Integer | Position size |
| entry_price | String | Entry price |
| leverage | String | Leverage (0 = cross margin) |
| cross_leverage_limit | String | Cross margin leverage |
| max_leverage | String | Max leverage under current risk limit |
| margin | String | Position margin |
| mode | String | Position mode |
| realised_pnl | String | Realised PnL |
| unrealised_pnl | String | Unrealised PnL |
| maintenance_rate | String | Maintenance rate |
| liq_price | String | Liquidation price |
| mark_price | String | Mark price |
| update_id | Integer | Update ID |
| update_time | Integer | Update time |
| adl_ranking | Integer | ADL ranking |
| pending_orders | Integer | Pending orders count |
| history_pnl | String | History PnL |
| history_point | String | History point |
| last_close_pnl | String | Last close PnL |

---

### 18. futures.position_adl_rank

ADL ranking updates.

---

### 19. futures.autoorders

Auto/conditional order updates. (Does NOT support SBE)

---

## Account Trade Channels (Login Required)

Requires `futures.login` first (see Authentication doc).

### 20. futures.order_place

Place single order. Equivalent to `POST /futures/{settle}/orders`.

```json
{
  "time": 123456,
  "channel": "futures.order_place",
  "event": "api",
  "payload": {
    "req_id": "unique_id",
    "req_param": {
      "contract": "BTC_USDT",
      "size": 10,
      "price": "74000",
      "tif": "gtc"
    },
    "req_header": {
      "x-gate-exptime": "1234567890123"
    }
  }
}
```

**Order fields:** Same as REST API order creation (contract, size, iceberg, price, close, reduce_only, tif, text, auto_size, stp_act, market_order_slip_ratio)

### 21. futures.order_batch_place

Batch order placement. `req_param` is array of order objects.

### 22. futures.order_cancel

Cancel single order by `order_id`.

### 23. futures.order_cancel_ids

Cancel multiple orders by ID list.

### 24. futures.order_cancel_cp

Cancel all matched open orders for a contract.

### 25. futures.order_amend

Amend existing order (size, price, etc).

### 26. futures.order_list

List orders. Equivalent to `GET /futures/{settle}/orders`.

### 27. futures.order_status

Query single order status.

---

## Complete Channel List

### Public (no auth):
1. `futures.ping` / `futures.pong`
2. `futures.system`
3. `futures.tickers`
4. `futures.trades`
5. `futures.book_ticker` ★
6. `futures.order_book` (legacy)
7. `futures.order_book_update`
8. `futures.obu` (v2, fastest)
9. `futures.candlesticks`
10. `futures.public_liquidates`
11. `futures.contract_stats`

### Private (auth):
12. `futures.orders`
13. `futures.usertrades`
14. `futures.liquidates`
15. `futures.auto_deleverages`
16. `futures.position_closes`
17. `futures.balances`
18. `futures.reduce_risk_limits`
19. `futures.positions`
20. `futures.position_adl_rank`
21. `futures.autoorders`

### Account Trade (login):
22. `futures.login`
23. `futures.order_place`
24. `futures.order_batch_place`
25. `futures.order_cancel`
26. `futures.order_cancel_ids`
27. `futures.order_cancel_cp`
28. `futures.order_amend`
29. `futures.order_list`
30. `futures.order_status`

---

## SBE (Simple Binary Encoding) Support

For ultra-low latency, some channels support SBE binary push:

**SBE WebSocket URL:** `wss://ws-testnet.gate.com/v4/ws/futures/usdt/sbe`

Add `?sbe_schema_id=1` parameter. Schema XML: `https://github.com/gate/gatews/blob/master/sbe/schemas/testnet/gate_fex_ws_testnet_latest.xml`

**SBE-supported channels:**

| Channel | Description |
|---------|-------------|
| `futures.trades` | Public trades |
| `futures.order_book` | Order book |
| `futures.order_book_update` | Incremental updates |
| `futures.book_ticker` | Best bid/ask |
| `futures.obu` | Order book v2 |
| `futures.candlesticks` | Candlesticks |
| `futures.tickers` | Tickers |
| `futures.usertrades` | User trades |
| `futures.positions` | Positions |
| `futures.orders` | Orders |

---

## Decimal Support (2025-12-09)

Add header `X-Gate-Decimal` when connecting to receive decimal values.

Affected fields (changed from integer to string):

| Channel | Fields |
|---------|--------|
| futures.trades | size |
| futures.tickers | total_size |
| futures.book_ticker | A, B |
| futures.order_book_update | a.s, b.s |
| futures.order_book | a.s, b.s |
| futures.obu | size |
| futures.candlesticks | v |
| futures.orders | iceberg, left, size |
| futures.usertrades | size |
| futures.positions | size |

---

## Changelog (Key entries)

| Date | Change |
|------|--------|
| 2026-03-30 | `futures.order_place` request payload corrections |
| 2026-02-09 | TestNet SBE support for some channels |
| 2026-02-04 | `futures.obu` TestNet added |
| 2026-01-07 | Added `market_order_slip_ratio` to orders/order_place |
| 2025-12-09 | Decimal support added (contracts as strings) |
| 2025-09-25 | `futures.order_book_update` updates |
| 2025-05-22 | Added `full` to `futures.order_book_update` |
| 2025-04-25 | Added `futures.order_cancel_ids` channel |
| 2025-04-18 | New depth levels for order_book channels |
| 2025-03-12 | Added `is_internal` in futures.trades |
| 2025-02-19 | Added `futures.public_liquidates` channel |
| 2024-11-18 | Added "20ms" interval for order_book_update |
| 2022-11-22 | Added `t` in book_ticker and order_book |
| 2021-03-10 | Added `futures.book_ticker` and `futures.order_book_update` channels |
