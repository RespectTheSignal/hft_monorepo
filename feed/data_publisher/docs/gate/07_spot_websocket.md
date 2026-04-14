# Gate.io Spot WebSocket v4

> Live: `wss://api.gateio.ws/ws/v4/`
> TestNet: `wss://ws-testnet.gate.com/v4/ws/spot`
> SDK: `https://github.com/gateio/gatews`

## Message Format

### Client Request

```json
{
  "time": 1611541000,
  "id": 123456789,
  "channel": "spot.orders",
  "event": "subscribe",
  "payload": ["BTC_USDT", "GT_USDT"],
  "auth": {
    "method": "api_key",
    "KEY": "xxx",
    "SIGN": "xxx"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| time | Integer | Yes | Unix seconds (max 60s gap from server) |
| id | Integer | No | Request ID (echoed in response) |
| channel | String | Yes | Channel name |
| auth | Object | No | Auth for private channels |
| event | String | Yes | `"subscribe"` / `"unsubscribe"` |
| payload | Any | No | Channel-specific params |

### Server Response

```json
{
  "time": 1611541000,
  "time_ms": 1611541000001,
  "channel": "spot.orders",
  "event": "update",
  "error": null,
  "result": { ... }
}
```

### Events
- `subscribe` - Client subscribes
- `unsubscribe` - Client unsubscribes
- `update` - Server pushes new data

### Error Codes

| Code | Message |
|------|---------|
| 1 | Invalid request body format |
| 2 | Invalid argument provided |
| 3 | Server side error |
| 4 | Authentication fail |

---

## System API

### Ping/Pong

```json
{"time": 1545404023, "channel": "spot.ping"}
// Response:
{"time": 1545404023, "channel": "spot.pong", "event": "", "error": null, "result": null}
```

### Service Upgrade Notification

```json
{"type": "upgrade", "msg": "The connection will soon be closed for a service upgrade. Please reconnect."}
```

---

## Public Channels

---

### 1. spot.tickers

Update speed: 1000ms

**Subscribe:**
```json
{"time": 1611541000, "channel": "spot.tickers", "event": "subscribe", "payload": ["BTC_USDT"]}
```

Multiple subscribe/unsubscribe calls supported. Earlier subscriptions not overridden.

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| currency_pair | String | Currency pair |
| last | String | Last price |
| lowest_ask | String | Best ask price |
| highest_bid | String | Best bid price |
| change_percentage | String | 24h change % |
| base_volume | String | Base currency volume |
| quote_volume | String | Quote currency volume |
| high_24h | String | 24h highest |
| low_24h | String | 24h lowest |

---

### 2. spot.trades

Update speed: realtime

**Subscribe:**
```json
{"time": 1611541000, "channel": "spot.trades", "event": "subscribe", "payload": ["BTC_USDT"]}
```

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| id | Integer | All-market trade ID |
| id_market | Integer | Per-market trade ID (continuous within market) |
| create_time | Integer | Trade timestamp (seconds) |
| create_time_ms | String | Trade timestamp (ms, with sub-ms decimals) |
| side | String | `"buy"` / `"sell"` (taker side) |
| currency_pair | String | Currency pair |
| amount | String | Trade amount |
| price | String | Trade price |
| range | String | Market trade range ("start-end") |

---

### 3. spot.candlesticks

**Subscribe:**
```json
{"time": 1611541000, "channel": "spot.candlesticks", "event": "subscribe", "payload": ["1m", "BTC_USDT"]}
```

Intervals: `"10s"`, `"1m"`, `"5m"`, `"15m"`, `"30m"`, `"1h"`, `"4h"`, `"8h"`, `"1d"`, `"7d"`

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | String | Unix timestamp (seconds) |
| v | String | Total volume |
| c | String | Close price |
| h | String | Highest price |
| l | String | Lowest price |
| o | String | Open price |
| n | String | Subscription name (`<interval>_<pair>`) |
| a | String | Base currency trading amount |
| w | Boolean | `true` = window closed |

---

### 4. spot.book_ticker ★ (Best Bid/Ask)

**Subscribe:**
```json
{"time": 1611541000, "channel": "spot.book_ticker", "event": "subscribe", "payload": ["BTC_USDT"]}
```

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| **t** | Integer | Update time (ms) |
| **u** | Integer | Order book update ID |
| **s** | String | Currency pair |
| **b** | String | Best bid price |
| **B** | String | Best bid amount |
| **a** | String | Best ask price |
| **A** | String | Best ask amount |

---

### 5. spot.order_book_update (Incremental)

**Subscribe:**
```json
{"time": 1611541000, "channel": "spot.order_book_update", "event": "subscribe", "payload": ["BTC_USDT", "100ms"]}
```

Intervals: `"20ms"`, `"100ms"`

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | Integer | Update time (ms) |
| full | Boolean | `true` = full depth snapshot (replace local depth) |
| l | String | Depth level |
| e | String | (Ignore this field) |
| E | Integer | Deprecated, use `t` |
| s | String | Currency pair |
| U | Integer | First update ID since last |
| u | Integer | Last update ID since last |
| b | Array | Changed bids `[[price, amount], ...]` |
| a | Array | Changed asks `[[price, amount], ...]` |

---

### 6. spot.order_book (Full Snapshots)

**Subscribe:**
```json
{"time": 1611541000, "channel": "spot.order_book", "event": "subscribe", "payload": ["BTC_USDT", "5", "100ms"]}
```

Levels: `"5"`, `"10"`, `"20"`, `"50"`, `"100"`
Intervals: `"100ms"`, `"1000ms"`

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | Integer | Update time (ms) |
| lastUpdateId | Integer | Snapshot update ID |
| s | String | Currency pair |
| l | String | Depth level |
| bids | Array | Top bids (high to low) `[[price, amount], ...]` |
| asks | Array | Top asks (low to high) `[[price, amount], ...]` |

---

### 7. spot.obu (Order Book V2)

**Subscribe:**
```json
{"time": 123456, "channel": "spot.obu", "event": "subscribe", "payload": ["ob.BTC_USDT.50"]}
```

Format: `ob.<PAIR>.<LEVEL>` - Levels: `50` (20ms), `400` (100ms)

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| t | Integer | Timestamp (ms) |
| full | Boolean | `true` = full snapshot |
| s | String | Depth stream name |
| U | Integer | Starting update ID |
| u | Integer | Ending update ID |
| b | Array | Bids `[[price, amount], ...]` |
| a | Array | Asks `[[price, amount], ...]` |

> Amount = 0 means remove entry.

---

## Private Channels (Auth Required)

---

### 8. spot.orders

Update speed: realtime

**Subscribe:**
```json
{
  "time": 1611541000,
  "channel": "spot.orders",
  "event": "subscribe",
  "payload": ["BTC_USDT"],
  "auth": {"method": "api_key", "KEY": "xxx", "SIGN": "xxx"}
}
```

Use `["!all"]` for all pairs.

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| id | String | Order ID |
| user | Integer | User ID |
| text | String | User-defined info |
| create_time | String | Creation time |
| create_time_ms | String | Creation time (ms) |
| update_time | String | Last modification time |
| update_time_ms | String | Last modification time (ms) |
| event | String | Order event |
| currency_pair | String | Currency pair |
| type | String | `"limit"`, `"market"`, `"limit_repay"`, `"market_repay"`, etc. |
| account | String | `"spot"`, `"margin"`, `"cross_margin"`, `"unified"` |
| side | String | `"buy"` / `"sell"` |
| amount | String | Trade amount |
| price | String | Order price |
| time_in_force | String | `"gtc"`, `"ioc"`, `"poc"` |
| left | String | Amount remaining |
| filled_total | String | Total filled (quote currency) |
| filled_amount | String | Currency transaction volume |
| avg_deal_price | String | Average transaction price |
| fee | String | Fee deducted |
| fee_currency | String | Fee currency |
| point_fee | String | Points fee |
| gt_fee | String | GT fee |
| gt_discount | Boolean | GT discount used? |
| rebated_fee | String | Rebated fee |
| rebated_fee_currency | String | Rebated fee currency |
| auto_repay | Boolean | Auto repay (cross margin) |
| auto_borrow | Boolean | Auto borrow |
| stp_id | Integer | STP group ID |
| stp_act | String | STP action |
| finish_as | String | How finished |
| amend_text | String | Amend text |
| slippage | String | Slippage (0.0001-0.05) |

---

### 9. spot.orders_v2 (Lite)

Same as `spot.orders` but excludes: `fee`, `point_fee`, `gt_fee`, `rebated_fee`.

---

### 10. spot.usertrades

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| id | Integer | All-market trade ID |
| user_id | Integer | User ID |
| order_id | String | Related order ID |
| currency_pair | String | Currency pair |
| create_time | Integer | Time (seconds) |
| create_time_ms | String | Time (ms) |
| side | String | `"buy"` / `"sell"` |
| amount | String | Trade amount |
| role | String | `"maker"` / `"taker"` |
| price | String | Trade price |
| fee | String | Fee |
| fee_currency | String | Fee currency |
| point_fee | String | Points fee |
| gt_fee | String | GT fee |
| text | String | User-defined info |
| id_market | Integer | Per-market trade ID |

---

### 11. spot.usertrades_v2 (Lite)

Same but excludes: `fee`, `point_fee`, `gt_fee`.

---

### 12. spot.balances

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| timestamp | String | Unix seconds |
| timestamp_ms | String | Unix ms |
| user | String | User ID |
| currency | String | Currency |
| change | String | Change amount |
| total | String | Total balance |
| available | String | Available |
| freeze | String | Locked amount |
| freeze_change | String | Locked change |
| change_type | String | Change type |

---

### 13. spot.margin_balances

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| timestamp | String | Unix seconds |
| timestamp_ms | String | Unix ms |
| user | String | User ID |
| currency_pair | String | Currency pair |
| currency | String | Currency |
| change | String | Change amount |
| available | String | Available |
| freeze | String | Locked amount |
| borrowed | String | Borrowed amount |
| interest | String | Unpaid interest |

---

### 14. spot.funding_balances

Fields: `timestamp`, `timestamp_ms`, `user`, `currency`, `change`, `freeze`, `lent`

---

### 15. spot.cross_balances

Same fields as `spot.balances`.

---

### 16. spot.cross_loan (Deprecated)

Cross margin loan updates. Fields: `timestamp`, `user`, `currency`, `change`, `total`, `available`, `borrowed`, `interest`

---

### 17. spot.priceorders

Trigger/conditional order updates.

**Notification fields:**

| Field | Type | Description |
|-------|------|-------------|
| market | String | Market name |
| uid | String | User ID |
| id | String | Order ID |
| trigger_price | String | Trigger price |
| trigger_rule | String | Trigger rule |
| trigger_expiration | Integer | Trigger expiration |
| price | String | Order price |
| amount | String | Amount |
| side | String | Side |
| order_type | String | Order type |
| is_stop_order | Boolean | Stop order? |
| stop_trigger_price | String | Stop trigger price |
| stop_trigger_rule | String | Stop trigger rule |
| stop_price | String | Stop price |
| ctime | String | Creation time |
| ftime | String | Finish time |

---

## Account Trade Channels (Login Required)

### 18. spot.login

Authentication for WebSocket trading.

### 19. spot.order_place

**Request:**
```json
{
  "time": 123456,
  "channel": "spot.order_place",
  "event": "api",
  "payload": {
    "req_id": "unique_id",
    "req_param": {
      "currency_pair": "BTC_USDT",
      "side": "buy",
      "type": "limit",
      "amount": "0.001",
      "price": "70000",
      "time_in_force": "gtc"
    }
  }
}
```

**Order fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| text | string | No | User-defined info |
| currency_pair | string | Yes | Currency pair |
| type | string | No | `"limit"`, `"market"` |
| account | string | No | `"spot"`, `"margin"`, `"unified"`, `"cross_margin"` |
| side | string | Yes | `"buy"` / `"sell"` |
| amount | string | Yes | Amount (base for limit, quote for market buy) |
| price | string | No | Price (required for limit) |
| time_in_force | string | No | `"gtc"`, `"ioc"`, `"poc"` |
| iceberg | string | No | Iceberg display size |
| auto_borrow | boolean | No | Auto borrow |
| auto_repay | boolean | No | Auto repay |
| stp_act | string | No | STP action |
| slippage | string | No | Max slippage (0.0001-0.05) |

### 20. spot.order_cancel

Cancel single order by `order_id` + `currency_pair`.

### 21. spot.order_cancel_ids

Cancel multiple by ID list.

### 22. spot.order_cancel_cp

Cancel all orders for currency pair.

### 23. spot.order_amend

Amend order (`amount`, `price`, `amend_text`).

### 24. spot.order_status

Query single order.

### 25. spot.order_list

List orders with filters.

---

## Complete Channel List

### Public (no auth):
1. `spot.ping` / `spot.pong`
2. `spot.tickers` (1000ms)
3. `spot.trades` (realtime)
4. `spot.trades_v2` (deprecated)
5. `spot.candlesticks`
6. `spot.book_ticker` ★
7. `spot.order_book_update` (20ms/100ms)
8. `spot.order_book` (100ms/1000ms)
9. `spot.obu` (20ms for 50-level, 100ms for 400-level)

### Private (auth):
10. `spot.orders` (realtime)
11. `spot.orders_v2` (lite)
12. `spot.usertrades` (realtime)
13. `spot.usertrades_v2` (lite)
14. `spot.balances`
15. `spot.margin_balances`
16. `spot.funding_balances`
17. `spot.cross_balances`
18. `spot.cross_loan` (deprecated)
19. `spot.priceorders`

### Account Trade (login):
20. `spot.login`
21. `spot.order_place`
22. `spot.order_cancel`
23. `spot.order_cancel_ids`
24. `spot.order_cancel_cp`
25. `spot.order_amend`
26. `spot.order_status`
27. `spot.order_list`

---

## Python Demo

```python
#!/usr/bin/env python
import hashlib, hmac, json, time, threading
from websocket import WebSocketApp

class GateWebSocketApp(WebSocketApp):
    def __init__(self, url, api_key, api_secret, **kwargs):
        super().__init__(url, **kwargs)
        self._api_key = api_key
        self._api_secret = api_secret

    def _request(self, channel, event=None, payload=None, auth_required=True):
        current_time = int(time.time())
        data = {"time": current_time, "channel": channel, "event": event, "payload": payload}
        if auth_required:
            message = 'channel=%s&event=%s&time=%d' % (channel, event, current_time)
            data['auth'] = {
                "method": "api_key",
                "KEY": self._api_key,
                "SIGN": hmac.new(
                    self._api_secret.encode("utf8"),
                    message.encode("utf8"),
                    hashlib.sha512
                ).hexdigest()
            }
        self.send(json.dumps(data))

    def subscribe(self, channel, payload=None, auth_required=True):
        self._request(channel, "subscribe", payload, auth_required)

    def unsubscribe(self, channel, payload=None, auth_required=True):
        self._request(channel, "unsubscribe", payload, auth_required)

def on_message(ws, message):
    print(f"Received: {message}")

def on_open(ws):
    ws.subscribe("spot.book_ticker", ["BTC_USDT"], False)

if __name__ == "__main__":
    app = GateWebSocketApp(
        "wss://api.gateio.ws/ws/v4/",
        "YOUR_API_KEY", "YOUR_API_SECRET",
        on_open=on_open, on_message=on_message
    )
    app.run_forever(ping_interval=5)
```

---

## Changelog (Key entries)

| Date | Change |
|------|--------|
| 2026-03-31 | `spot.obu` TestNet first snapshot push |
| 2026-01-07 | Added `slippage` to order_place/orders |
| 2025-10-21 | Updated `type` description in orders |
| 2025-06-24 | Added Order Book V2 (`spot.obu`) |
| 2025-04-10 | Added `filled_amount` in orders |
| 2025-02-10 | Added spot.order_list, rate limit headers |
| 2025-01-08 | Added `id_market`, v2 channels |
| 2024-11-28 | Removed 1000ms from order_book_update |
