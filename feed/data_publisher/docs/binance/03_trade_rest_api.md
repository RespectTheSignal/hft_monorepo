# Binance USDⓈ-M Futures Trade REST API

All trade endpoints require `TRADE` or `USER_DATA` security type (API Key + HMAC signature).
Base URL: `https://fapi.binance.com`

---

## 1. New Order

Send a new order.

| Field | Value |
|-------|-------|
| Endpoint | `POST /fapi/v1/order` |
| Security | TRADE |
| Weight (IP) | 0 |
| Weight (Order 10s) | 1 |
| Weight (Order 1m) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair (e.g. `BTCUSDT`) |
| side | ENUM | YES | `BUY` or `SELL` |
| positionSide | ENUM | NO | Default `BOTH` for One-way Mode; `LONG` or `SHORT` required in Hedge Mode |
| type | ENUM | YES | Order type: `LIMIT`, `MARKET`, `STOP`, `STOP_MARKET`, `TAKE_PROFIT`, `TAKE_PROFIT_MARKET`, `TRAILING_STOP_MARKET` |
| timeInForce | ENUM | NO | `GTC`, `IOC`, `FOK`, `GTX`, `GTD` |
| quantity | DECIMAL | NO | Order quantity. Cannot be sent with `closePosition=true` |
| reduceOnly | STRING | NO | `"true"` or `"false"`, default `"false"`. Cannot be used in Hedge Mode; cannot be `true` with `closePosition=true` |
| price | DECIMAL | NO | Limit price |
| newClientOrderId | STRING | NO | Unique ID; pattern: `^[\.A-Z\:/a-z0-9_-]{1,36}$`; auto-generated if omitted |
| stopPrice | DECIMAL | NO | Trigger price for `STOP`, `STOP_MARKET`, `TAKE_PROFIT`, `TAKE_PROFIT_MARKET` |
| closePosition | STRING | NO | `"true"` or `"false"`. Close-All with `STOP_MARKET` or `TAKE_PROFIT_MARKET` |
| activationPrice | DECIMAL | NO | For `TRAILING_STOP_MARKET`; defaults to latest price if omitted |
| callbackRate | DECIMAL | NO | For `TRAILING_STOP_MARKET`; min 0.1, max 5 (percentage) |
| workingType | ENUM | NO | `MARK_PRICE` or `CONTRACT_PRICE`; default `CONTRACT_PRICE` |
| priceProtect | STRING | NO | `"TRUE"` or `"FALSE"`; default `"FALSE"`. Conditional order trigger protection |
| newOrderRespType | ENUM | NO | `"ACK"` or `"RESULT"`; default `"ACK"` |
| priceMatch | ENUM | NO | Only for `LIMIT`/`STOP`/`TAKE_PROFIT`: `OPPONENT`, `OPPONENT_5`, `OPPONENT_10`, `OPPONENT_20`, `QUEUE`, `QUEUE_5`, `QUEUE_10`, `QUEUE_20`. Cannot be sent with `price` |
| selfTradePreventionMode | ENUM | NO | `EXPIRE_TAKER`, `EXPIRE_MAKER`, `EXPIRE_BOTH`; default `EXPIRE_MAKER`. Only effective with `timeInForce` IOC, GTC, or GTD |
| goodTillDate | LONG | NO | Required when `timeInForce=GTD`. Auto-cancel timestamp (ms); precision to seconds; must exceed `serverTime + 600s` and be < 253402300799000 |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Additional Mandatory Parameters by Order Type

| Type | Mandatory Parameters |
|------|---------------------|
| LIMIT | `timeInForce`, `quantity`, `price` |
| MARKET | `quantity` |
| STOP | `quantity`, `price`, `stopPrice` |
| STOP_MARKET | `stopPrice` |
| TAKE_PROFIT | `quantity`, `price`, `stopPrice` |
| TAKE_PROFIT_MARKET | `stopPrice` |
| TRAILING_STOP_MARKET | `callbackRate` |

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| clientOrderId | STRING | Client order identifier |
| cumQty | STRING | Cumulative executed quantity |
| cumQuote | STRING | Cumulative quote asset value |
| executedQty | STRING | Total filled quantity |
| orderId | LONG | Binance order ID |
| avgPrice | STRING | Average execution price |
| origQty | STRING | Original order quantity |
| price | STRING | Limit order price |
| reduceOnly | BOOLEAN | Reduce-only flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG`, `SHORT`, or `BOTH` |
| status | STRING | Order status |
| stopPrice | STRING | Stop trigger price (empty for `TRAILING_STOP_MARKET`) |
| closePosition | BOOLEAN | Close-All indicator |
| symbol | STRING | Trading pair |
| timeInForce | STRING | Time-in-force applied |
| type | STRING | Order type |
| origType | STRING | Original order type |
| activatePrice | STRING | Activation price (`TRAILING_STOP_MARKET` only) |
| priceRate | STRING | Callback rate (`TRAILING_STOP_MARKET` only) |
| updateTime | LONG | Last update timestamp (ms) |
| workingType | STRING | Working price type |
| priceProtect | BOOLEAN | Conditional order trigger protection |
| priceMatch | STRING | Price matching mode |
| selfTradePreventionMode | STRING | STP mode applied |
| goodTillDate | LONG | Auto-cancel timestamp for GTD orders (ms) |

### Notes

- When `newOrderRespType` is `RESULT`: MARKET orders return final FILLED status immediately; LIMIT orders with special `timeInForce` return final status (FILLED or EXPIRED) directly
- Under extreme market conditions, GTD order cancellation may lag beyond `goodTillDate`
- `STOP_MARKET` and `TAKE_PROFIT_MARKET` with `closePosition=true` will close all positions; `quantity` cannot be sent
- Condition orders are triggered when price reaches `stopPrice`:
  - `STOP`/`STOP_MARKET`: trigger when price >= stopPrice (BUY) or <= stopPrice (SELL)
  - `TAKE_PROFIT`/`TAKE_PROFIT_MARKET`: trigger when price <= stopPrice (BUY) or >= stopPrice (SELL)

---

## 2. New Order Test

Test new order creation. Validates parameters but does NOT submit to the matching engine.

| Field | Value |
|-------|-------|
| Endpoint | `POST /fapi/v1/order/test` |
| Security | TRADE |
| Weight (IP) | 0 |
| Weight (Order 10s) | 1 |
| Weight (Order 1m) | 1 |

### Request Parameters

Same as [New Order](#1-new-order) -- all parameters identical.

### Response Fields

Same as [New Order](#1-new-order) -- but order is not actually placed.

---

## 3. Place Multiple Orders

Place up to 5 orders in a single batch request.

| Field | Value |
|-------|-------|
| Endpoint | `POST /fapi/v1/batchOrders` |
| Security | TRADE |
| Weight (IP) | 5 |
| Weight (Order 10s) | 5 |
| Weight (Order 1m) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| batchOrders | LIST\<JSON\> | YES | Order list. Max 5 orders |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

#### Each order object in `batchOrders`:

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| side | ENUM | YES | `BUY` or `SELL` |
| positionSide | ENUM | NO | Default `BOTH`; `LONG`/`SHORT` for Hedge Mode |
| type | ENUM | YES | Order type |
| timeInForce | ENUM | NO | Time-in-force |
| quantity | DECIMAL | YES | Order quantity |
| reduceOnly | STRING | NO | `"true"` or `"false"`, default `"false"` |
| price | DECIMAL | NO | Order price |
| newClientOrderId | STRING | NO | Pattern: `^[\.A-Z\:/a-z0-9_-]{1,36}$` |
| stopPrice | DECIMAL | NO | Stop trigger price |
| activationPrice | DECIMAL | NO | For `TRAILING_STOP_MARKET` |
| callbackRate | DECIMAL | NO | For `TRAILING_STOP_MARKET` |
| workingType | ENUM | NO | `MARK_PRICE` or `CONTRACT_PRICE` |
| priceProtect | STRING | NO | `"TRUE"` or `"FALSE"` |
| newOrderRespType | ENUM | NO | `"ACK"` or `"RESULT"`, default `"ACK"` |
| priceMatch | ENUM | NO | `OPPONENT`/`OPPONENT_5`/`OPPONENT_10`/`OPPONENT_20`/`QUEUE`/`QUEUE_5`/`QUEUE_10`/`QUEUE_20` |
| selfTradePreventionMode | ENUM | NO | `EXPIRE_TAKER`/`EXPIRE_MAKER`/`EXPIRE_BOTH`; default `NONE` |
| goodTillDate | LONG | NO | For `timeInForce=GTD`; timestamp (second-level precision) |

### Response Fields

Returns a JSON array. Each element has the same fields as [New Order response](#response-fields) on success, or error fields on failure:

| Field | Type | Description |
|-------|------|-------------|
| code | INT | Error code (only on failure) |
| msg | STRING | Error message (only on failure) |

### Notes

- Batch orders are processed concurrently; matching order is NOT guaranteed
- Response array order matches request order
- Maximum 5 orders per request
- Each failed order returns individual error code/message

---

## 4. Modify Order

Modify an existing LIMIT order (price and/or quantity).

| Field | Value |
|-------|-------|
| Endpoint | `PUT /fapi/v1/order` |
| Security | TRADE |
| Weight (IP) | 0 |
| Weight (Order 10s) | 1 |
| Weight (Order 1m) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| orderId | LONG | NO | Order ID to modify |
| origClientOrderId | STRING | NO | Client order ID to modify |
| symbol | STRING | YES | Trading pair |
| side | ENUM | YES | `BUY` or `SELL` |
| quantity | DECIMAL | YES | New order quantity. Cannot be sent with `closePosition=true` |
| price | DECIMAL | YES | New order price |
| priceMatch | ENUM | NO | `OPPONENT`/`OPPONENT_5`/`OPPONENT_10`/`OPPONENT_20`/`QUEUE`/`QUEUE_5`/`QUEUE_10`/`QUEUE_20`. Only for `LIMIT`/`STOP`/`TAKE_PROFIT` |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| orderId | LONG | Order ID |
| symbol | STRING | Trading pair |
| pair | STRING | Pair notation |
| status | STRING | Order status (e.g. `NEW`) |
| clientOrderId | STRING | Client order ID |
| price | STRING | Order price |
| avgPrice | STRING | Average execution price |
| origQty | STRING | Original quantity |
| executedQty | STRING | Executed quantity |
| cumQty | STRING | Cumulative quantity |
| cumBase | STRING | Cumulative base amount |
| timeInForce | STRING | Time-in-force |
| type | STRING | Order type |
| reduceOnly | BOOLEAN | Reduce-only flag |
| closePosition | BOOLEAN | Close position flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | Position side |
| stopPrice | STRING | Stop price |
| workingType | STRING | Working price type |
| priceProtect | BOOLEAN | Price protection status |
| origType | STRING | Original order type |
| priceMatch | STRING | Price matching mode |
| selfTradePreventionMode | STRING | STP mode |
| goodTillDate | LONG | GTD auto-cancel timestamp |
| updateTime | LONG | Last update timestamp |

### Notes

- Either `orderId` or `origClientOrderId` must be sent; `orderId` prevails if both sent
- Both `quantity` and `price` must be sent (differs from COIN-M futures modify)
- Currently only supports LIMIT order modification
- Modified orders re-enter the match queue
- If new `quantity` <= `executedQty` (partial fill scenario), order is cancelled
- If GTX order would execute immediately after modification, order is cancelled
- Maximum 10,000 modifications per order

---

## 5. Query Order

Check an order's status.

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v1/order` |
| Security | USER_DATA |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | Binance order ID |
| origClientOrderId | STRING | NO | Client order ID |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

Either `orderId` or `origClientOrderId` must be sent.

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| avgPrice | STRING | Average execution price |
| clientOrderId | STRING | Client order ID |
| cumQuote | STRING | Cumulative quote amount |
| executedQty | STRING | Executed quantity |
| orderId | LONG | Order ID |
| origQty | STRING | Original order quantity |
| origType | STRING | Original order type |
| price | STRING | Order price |
| reduceOnly | BOOLEAN | Reduce-only flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG` or `SHORT` |
| status | STRING | Order status |
| stopPrice | STRING | Stop trigger price |
| closePosition | BOOLEAN | Close-All indicator |
| symbol | STRING | Trading pair |
| time | LONG | Order creation timestamp (ms) |
| timeInForce | STRING | Time-in-force |
| type | STRING | Order type |
| activatePrice | STRING | Activation price (`TRAILING_STOP_MARKET` only) |
| priceRate | STRING | Callback rate (`TRAILING_STOP_MARKET` only) |
| updateTime | LONG | Last update timestamp (ms) |
| workingType | STRING | Price reference type |
| priceProtect | BOOLEAN | Conditional trigger protection flag |

### Notes

- Orders become inaccessible after: canceled/expired with zero fills and 3+ days elapsed, or 90+ days from creation

---

## 6. Cancel Order

Cancel an active order.

| Field | Value |
|-------|-------|
| Endpoint | `DELETE /fapi/v1/order` |
| Security | TRADE |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | Binance order ID |
| origClientOrderId | STRING | NO | Client order ID |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

Either `orderId` or `origClientOrderId` must be sent.

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| clientOrderId | STRING | Client order ID |
| cumQty | STRING | Cumulative executed quantity |
| cumQuote | STRING | Cumulative quote asset value |
| executedQty | STRING | Total executed quantity |
| orderId | LONG | Order ID |
| origQty | STRING | Original order quantity |
| origType | STRING | Original order type |
| price | STRING | Order price |
| avgPrice | STRING | Average execution price |
| reduceOnly | BOOLEAN | Reduce-only flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG` or `SHORT` |
| status | STRING | Order status (e.g. `CANCELED`) |
| stopPrice | STRING | Stop trigger price |
| closePosition | BOOLEAN | Close-All indicator |
| symbol | STRING | Trading pair |
| timeInForce | STRING | Time-in-force |
| type | STRING | Order type |
| activatePrice | STRING | Activation price (`TRAILING_STOP_MARKET` only) |
| priceRate | STRING | Callback rate (`TRAILING_STOP_MARKET` only) |
| updateTime | LONG | Last update timestamp (ms) |
| workingType | STRING | Working price type |
| priceProtect | BOOLEAN | Conditional trigger protection flag |
| priceMatch | STRING | Price matching mode |
| selfTradePreventionMode | STRING | STP mode |
| goodTillDate | LONG | GTD auto-cancel timestamp |

---

## 7. Cancel All Open Orders

Cancel all open orders for a symbol.

| Field | Value |
|-------|-------|
| Endpoint | `DELETE /fapi/v1/allOpenOrders` |
| Security | TRADE |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response

```json
{
  "code": 200,
  "msg": "The operation of cancel all open order is done."
}
```

| Field | Type | Description |
|-------|------|-------------|
| code | INT | Status code (200 = success) |
| msg | STRING | Status message |

---

## 8. Cancel Multiple Orders

Cancel a batch of orders by ID.

| Field | Value |
|-------|-------|
| Endpoint | `DELETE /fapi/v1/batchOrders` |
| Security | TRADE |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderIdList | LIST\<LONG\> | NO | Max 10. e.g. `[1234567,2345678]` |
| origClientOrderIdList | LIST\<STRING\> | NO | Max 10. e.g. `["my_id_1","my_id_2"]` (encode double quotes, no space after comma) |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

Either `orderIdList` or `origClientOrderIdList` must be sent.

### Response Fields

Returns a JSON array. Each element on success:

| Field | Type | Description |
|-------|------|-------------|
| clientOrderId | STRING | Client order ID |
| cumQty | STRING | Cumulative filled quantity |
| cumQuote | STRING | Cumulative quote amount |
| executedQty | STRING | Executed quantity |
| orderId | LONG | Order ID |
| origQty | STRING | Original order quantity |
| origType | STRING | Original order type |
| price | STRING | Order price |
| reduceOnly | BOOLEAN | Reduce-only flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG` or `SHORT` |
| status | STRING | Order status |
| stopPrice | STRING | Stop price (empty for `TRAILING_STOP_MARKET`) |
| closePosition | BOOLEAN | Close-All indicator |
| symbol | STRING | Trading pair |
| timeInForce | STRING | Time-in-force |
| type | STRING | Order type |
| activatePrice | STRING | Activation price (`TRAILING_STOP_MARKET` only) |
| priceRate | STRING | Callback rate (`TRAILING_STOP_MARKET` only) |
| updateTime | LONG | Last update timestamp (ms) |
| workingType | STRING | Working price type |
| priceProtect | BOOLEAN | Conditional trigger protection flag |
| priceMatch | STRING | Price matching mode |
| selfTradePreventionMode | STRING | STP mode |
| goodTillDate | LONG | GTD auto-cancel timestamp |

On failure, each element returns:

| Field | Type | Description |
|-------|------|-------------|
| code | INT | Error code |
| msg | STRING | Error message |

---

## 9. Auto-Cancel All Open Orders (Countdown)

Set a countdown timer that auto-cancels all open orders for a symbol. Must be called repeatedly as a heartbeat to reset the timer.

| Field | Value |
|-------|-------|
| Endpoint | `POST /fapi/v1/countdownCancelAll` |
| Security | TRADE |
| Weight (IP) | 10 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| countdownTime | LONG | YES | Countdown in milliseconds. 1000 = 1 second. 0 = cancel the timer |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| countdownTime | STRING | Countdown time applied (ms) |

### Response Example

```json
{
  "symbol": "BTCUSDT",
  "countdownTime": "100000"
}
```

### Notes

- Endpoint must be called repeatedly to reset the countdown (heartbeat mechanism)
- System checks all countdowns approximately every 10 milliseconds
- Do not set countdown time too precisely or too small
- Pass `countdownTime=0` to cancel an active countdown

---

## 10. Query Current Open Order

Query a single active open order.

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v1/openOrder` |
| Security | USER_DATA |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | Order ID |
| origClientOrderId | STRING | NO | Client order ID |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

Either `orderId` or `origClientOrderId` must be sent. Returns "Order does not exist" error if the order has already been filled or cancelled.

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| avgPrice | STRING | Average execution price |
| clientOrderId | STRING | Client order ID |
| cumQuote | STRING | Cumulative quote amount |
| executedQty | STRING | Executed quantity |
| orderId | LONG | Order ID |
| origQty | STRING | Original order quantity |
| origType | STRING | Original order type |
| price | STRING | Order price |
| reduceOnly | BOOLEAN | Reduce-only flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG` or `SHORT` |
| status | STRING | Order status |
| stopPrice | STRING | Stop price (empty for `TRAILING_STOP_MARKET`) |
| closePosition | BOOLEAN | Close-All flag |
| symbol | STRING | Trading pair |
| time | LONG | Order creation timestamp (ms) |
| timeInForce | STRING | Time-in-force |
| type | STRING | Order type |
| activatePrice | STRING | Activation price (`TRAILING_STOP_MARKET` only) |
| priceRate | STRING | Callback rate (`TRAILING_STOP_MARKET` only) |
| updateTime | LONG | Last update timestamp (ms) |
| workingType | STRING | Working price type |
| priceProtect | BOOLEAN | Conditional trigger protection flag |
| priceMatch | STRING | Price matching mode |
| selfTradePreventionMode | STRING | STP mode |
| goodTillDate | LONG | Auto-cancel timestamp for GTD orders |

---

## 11. Current All Open Orders

Get all open orders on a symbol (or all symbols).

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v1/openOrders` |
| Security | USER_DATA |
| Weight (IP) | 1 with symbol; 40 without symbol |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | NO | Trading pair. If omitted, returns orders for ALL symbols |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

Returns a JSON array. Each element:

| Field | Type | Description |
|-------|------|-------------|
| avgPrice | STRING | Average execution price |
| clientOrderId | STRING | Client order ID |
| cumQuote | STRING | Cumulative quote amount |
| executedQty | STRING | Executed quantity |
| orderId | LONG | Order ID |
| origQty | STRING | Original order quantity |
| origType | STRING | Original order type |
| price | STRING | Order price |
| reduceOnly | BOOLEAN | Reduce-only flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG` or `SHORT` |
| status | STRING | Order status (e.g. `NEW`) |
| stopPrice | STRING | Stop trigger price |
| closePosition | BOOLEAN | Close-All indicator |
| symbol | STRING | Trading pair |
| time | LONG | Order creation timestamp (ms) |
| timeInForce | STRING | Time-in-force (e.g. `GTC`) |
| type | STRING | Order type |
| activatePrice | STRING | Activation price (`TRAILING_STOP_MARKET` only) |
| priceRate | STRING | Callback rate (`TRAILING_STOP_MARKET` only) |
| updateTime | LONG | Last update timestamp (ms) |
| workingType | STRING | Price reference type |
| priceProtect | BOOLEAN | Conditional trigger protection |
| priceMatch | STRING | Price matching mode |
| selfTradePreventionMode | STRING | STP mode |
| goodTillDate | LONG | GTD auto-cancel timestamp |

### Notes

- Use caution when calling without `symbol` (weight: 40)

---

## 12. All Orders

Get all account orders (active, canceled, or filled).

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v1/allOrders` |
| Security | USER_DATA |
| Weight (IP) | 5 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | If set, returns orders >= this orderId; otherwise returns most recent |
| startTime | LONG | NO | Start timestamp (ms) |
| endTime | LONG | NO | End timestamp (ms) |
| limit | INT | NO | Default 500; max 1000 |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

Returns a JSON array. Each element:

| Field | Type | Description |
|-------|------|-------------|
| avgPrice | STRING | Average execution price |
| clientOrderId | STRING | Client order ID |
| cumQuote | STRING | Cumulative quote amount |
| executedQty | STRING | Executed quantity |
| orderId | LONG | Order ID |
| origQty | STRING | Original order quantity |
| origType | STRING | Original order type |
| price | STRING | Order price |
| reduceOnly | BOOLEAN | Reduce-only flag |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG` or `SHORT` |
| status | STRING | Order status |
| stopPrice | STRING | Stop price (empty for `TRAILING_STOP_MARKET`) |
| closePosition | BOOLEAN | Close-All indicator |
| symbol | STRING | Trading pair |
| time | LONG | Order creation timestamp (ms) |
| timeInForce | STRING | Time-in-force |
| type | STRING | Order type |
| activatePrice | STRING | Activation price (`TRAILING_STOP_MARKET` only) |
| priceRate | STRING | Callback rate (`TRAILING_STOP_MARKET` only) |
| updateTime | LONG | Last update timestamp (ms) |
| workingType | STRING | Price reference type |
| priceProtect | BOOLEAN | Conditional trigger protection flag |
| priceMatch | STRING | Price matching mode |
| selfTradePreventionMode | STRING | STP mode |
| goodTillDate | LONG | GTD auto-cancel timestamp |

### Notes

- Query time period must be less than 7 days (default: recent 7 days)
- Excluded from results: canceled/expired orders with zero fills created 3+ days ago, or orders created 90+ days ago

---

## 13. Account Trade List

Get trades for a specific account and symbol.

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v1/userTrades` |
| Security | USER_DATA |
| Weight (IP) | 5 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | Filter by order ID. Can only be used with `symbol` |
| startTime | LONG | NO | Start timestamp (ms) |
| endTime | LONG | NO | End timestamp (ms) |
| fromId | LONG | NO | Trade ID to fetch from. Default: most recent trades. Cannot combine with `startTime`/`endTime` |
| limit | INT | NO | Default 500; max 1000 |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

Returns a JSON array. Each element:

| Field | Type | Description |
|-------|------|-------------|
| buyer | BOOLEAN | Whether account was buyer |
| commission | STRING | Commission amount (negative = deduction) |
| commissionAsset | STRING | Commission asset (e.g. `USDT`) |
| id | LONG | Trade ID |
| maker | BOOLEAN | Whether account was maker |
| orderId | LONG | Associated order ID |
| price | STRING | Execution price |
| qty | STRING | Trade quantity |
| quoteQty | STRING | Quote asset quantity |
| realizedPnl | STRING | Realized profit/loss |
| side | STRING | `BUY` or `SELL` |
| positionSide | STRING | `LONG` or `SHORT` |
| symbol | STRING | Trading pair |
| time | LONG | Trade execution timestamp (ms) |

### Notes

- If neither `startTime` nor `endTime` provided, returns last 7 days of data
- Time range cannot exceed 7 days
- `fromId` cannot be combined with `startTime` or `endTime`
- Only supports querying trades in the past 6 months

---

## 14. Get Order Modify History

Get modification history for an order.

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v1/orderAmendment` |
| Security | USER_DATA |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | Order ID |
| origClientOrderId | STRING | NO | Client order ID |
| startTime | LONG | NO | Start timestamp (ms), inclusive |
| endTime | LONG | NO | End timestamp (ms), inclusive |
| limit | INT | NO | Default 50; max 100 |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

Either `orderId` or `origClientOrderId` must be provided; `orderId` takes precedence if both sent.

### Response Fields

Returns a JSON array. Each element:

| Field | Type | Description |
|-------|------|-------------|
| amendmentId | LONG | Unique modification ID |
| symbol | STRING | Trading pair |
| pair | STRING | Pair notation |
| orderId | LONG | Order ID |
| clientOrderId | STRING | Client order ID |
| time | LONG | Modification timestamp (ms) |
| amendment.price.before | STRING | Price before modification |
| amendment.price.after | STRING | Price after modification |
| amendment.origQty.before | STRING | Quantity before modification |
| amendment.origQty.after | STRING | Quantity after modification |
| amendment.count | INT | Number of times the order has been modified |

### Notes

- Order modify history older than 3 months is not available

---

## 15. Change Initial Leverage

Change the initial leverage for a symbol.

| Field | Value |
|-------|-------|
| Endpoint | `POST /fapi/v1/leverage` |
| Security | TRADE |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| leverage | INT | YES | Target initial leverage: 1 to 125 |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| leverage | INT | Confirmed leverage level |
| maxNotionalValue | STRING | Maximum notional value at this leverage |
| symbol | STRING | Trading pair |

### Response Example

```json
{
  "leverage": 21,
  "maxNotionalValue": "1000000",
  "symbol": "BTCUSDT"
}
```

---

## 16. Change Margin Type

Change the margin type for a symbol (isolated or cross).

| Field | Value |
|-------|-------|
| Endpoint | `POST /fapi/v1/marginType` |
| Security | TRADE |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| marginType | ENUM | YES | `ISOLATED` or `CROSSED` |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| code | INT | Status code (200 = success) |
| msg | STRING | Status message |

### Response Example

```json
{
  "code": 200,
  "msg": "success"
}
```

---

## 17. Modify Isolated Position Margin

Add or reduce margin for an isolated position.

| Field | Value |
|-------|-------|
| Endpoint | `POST /fapi/v1/positionMargin` |
| Security | TRADE |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| positionSide | ENUM | NO | Default `BOTH` for One-way Mode; `LONG` or `SHORT` required in Hedge Mode |
| amount | DECIMAL | YES | Margin adjustment amount |
| type | INT | YES | `1` = add margin, `2` = reduce margin |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| amount | DECIMAL | Modified margin amount |
| code | INT | Status code (200 = success) |
| msg | STRING | Status message |
| type | INT | Operation type (1 or 2) |

### Notes

- Only works for isolated margin positions

---

## 18. Get Position Margin Change History

Query margin change history for isolated positions.

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v1/positionMargin/history` |
| Security | USER_DATA |
| Weight (IP) | 1 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| type | INT | NO | `1` = add margin, `2` = reduce margin |
| startTime | LONG | NO | Start timestamp (ms) |
| endTime | LONG | NO | End timestamp (ms); defaults to current time |
| limit | INT | NO | Default 500 |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

Returns a JSON array. Each element:

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| type | INT | Operation type (1 = add, 2 = reduce) |
| deltaType | STRING | Change category (e.g. `USER_ADJUST`) |
| amount | STRING | Margin adjustment quantity |
| asset | STRING | Asset denomination |
| time | LONG | Transaction timestamp (ms) |
| positionSide | STRING | `BOTH`, `LONG`, or `SHORT` |

### Notes

- Historical data available for up to 30 days prior
- Time range between `startTime` and `endTime` cannot exceed 30 days

---

## 19. Position Information V2

Get current position information.

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v2/positionRisk` |
| Security | USER_DATA |
| Weight (IP) | 5 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | NO | Trading pair. If omitted, returns all symbols |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

Returns a JSON array. Each element:

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| positionAmt | STRING | Position quantity |
| entryPrice | STRING | Average entry price |
| breakEvenPrice | STRING | Break-even price |
| markPrice | STRING | Current mark price |
| unRealizedProfit | STRING | Unrealized PnL |
| liquidationPrice | STRING | Liquidation price |
| leverage | STRING | Leverage multiplier |
| maxNotionalValue | STRING | Max notional value allowed |
| marginType | STRING | `isolated` or `cross` |
| isolatedMargin | STRING | Isolated margin amount |
| isAutoAddMargin | STRING | Auto-add-margin status (`"true"`/`"false"`) |
| positionSide | STRING | `BOTH`, `LONG`, or `SHORT` |
| notional | STRING | Position notional value |
| isolatedWallet | STRING | Isolated wallet balance |
| updateTime | LONG | Last update timestamp (ms) |

### Notes

- Use with user data stream `ACCOUNT_UPDATE` for optimal timeliness and accuracy
- One-way mode: single position per symbol with `positionSide=BOTH`
- Hedge mode: up to two positions per symbol (separate LONG and SHORT)

---

## 20. Position Information V3

Get current position information (V3 with additional fields).

| Field | Value |
|-------|-------|
| Endpoint | `GET /fapi/v3/positionRisk` |
| Security | USER_DATA |
| Weight (IP) | 5 |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | NO | Trading pair. If omitted, returns all symbols |
| recvWindow | LONG | NO | Request validity window (ms) |
| timestamp | LONG | YES | Request timestamp (ms) |

### Response Fields

Returns a JSON array. Each element:

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| positionSide | STRING | `BOTH`, `LONG`, or `SHORT` |
| positionAmt | STRING | Position quantity |
| entryPrice | STRING | Average entry price |
| breakEvenPrice | STRING | Break-even price |
| markPrice | STRING | Current mark price |
| unRealizedProfit | STRING | Unrealized PnL |
| liquidationPrice | STRING | Liquidation price |
| isolatedMargin | STRING | Isolated margin balance |
| notional | STRING | Position notional value |
| marginAsset | STRING | Margin currency (e.g. `USDT`) |
| isolatedWallet | STRING | Isolated wallet balance |
| initialMargin | STRING | Required margin at current mark price |
| maintMargin | STRING | Maintenance margin requirement |
| positionInitialMargin | STRING | Initial margin for positions |
| openOrderInitialMargin | STRING | Initial margin for open orders |
| adl | INT | Auto-Deleveraging quantile |
| bidNotional | STRING | Bid-side notional |
| askNotional | STRING | Ask-side notional |
| updateTime | LONG | Last update timestamp (ms) |

### Notes

- Use with user data stream `ACCOUNT_UPDATE` for optimal timeliness and accuracy
- Only positions with active holdings or open orders are included in responses

---

## Enums Reference

### Order Side
- `BUY`
- `SELL`

### Position Side
- `BOTH` (One-way mode)
- `LONG` (Hedge mode)
- `SHORT` (Hedge mode)

### Order Type
- `LIMIT`
- `MARKET`
- `STOP`
- `STOP_MARKET`
- `TAKE_PROFIT`
- `TAKE_PROFIT_MARKET`
- `TRAILING_STOP_MARKET`

### Time in Force
- `GTC` - Good Till Cancel
- `IOC` - Immediate or Cancel
- `FOK` - Fill or Kill
- `GTX` - Good Till Crossing (Post Only)
- `GTD` - Good Till Date

### Order Status
- `NEW`
- `PARTIALLY_FILLED`
- `FILLED`
- `CANCELED`
- `EXPIRED`
- `NEW_INSURANCE` - Liquidation with insurance fund
- `NEW_ADL` - Counterparty liquidation

### Working Type
- `MARK_PRICE`
- `CONTRACT_PRICE`

### Response Type
- `ACK`
- `RESULT`

### Margin Type
- `ISOLATED`
- `CROSSED`

### Self Trade Prevention Mode
- `NONE`
- `EXPIRE_TAKER`
- `EXPIRE_MAKER`
- `EXPIRE_BOTH`

### Price Match
- `NONE`
- `OPPONENT`
- `OPPONENT_5`
- `OPPONENT_10`
- `OPPONENT_20`
- `QUEUE`
- `QUEUE_5`
- `QUEUE_10`
- `QUEUE_20`
