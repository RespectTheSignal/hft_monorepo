# Binance USDⓈ-M Futures Account REST API

> Base URL: `https://fapi.binance.com`
> All endpoints in this section require `USER_DATA` security (API key + HMAC signature).
> All timestamps are in milliseconds.

---

## Table of Contents

1. [Futures Account Balance V3](#1-futures-account-balance-v3)
2. [Futures Account Balance V2](#2-futures-account-balance-v2)
3. [Account Information V3](#3-account-information-v3)
4. [Account Information V2](#4-account-information-v2)
5. [Get Future Account Transaction History List](#5-get-future-account-transaction-history-list)
6. [User Commission Rate](#6-user-commission-rate)
7. [Query Account Configuration](#7-query-account-configuration)
8. [Query Symbol Configuration](#8-query-symbol-configuration)
9. [Query Order Rate Limit](#9-query-order-rate-limit)
10. [Notional and Leverage Brackets](#10-notional-and-leverage-brackets)
11. [Get Current Multi-Assets Mode](#11-get-current-multi-assets-mode)
12. [Get Current Position Mode](#12-get-current-position-mode)
13. [Get Income History](#13-get-income-history)
14. [Futures Trading Quantitative Rules Indicators](#14-futures-trading-quantitative-rules-indicators)
15. [Get Download Id For Futures Transaction History](#15-get-download-id-for-futures-transaction-history)
16. [Get Futures Transaction History Download Link By Id](#16-get-futures-transaction-history-download-link-by-id)
17. [Get Download Id For Futures Order History](#17-get-download-id-for-futures-order-history)
18. [Get Futures Order History Download Link By Id](#18-get-futures-order-history-download-link-by-id)
19. [Get Download Id For Futures Trade History](#19-get-download-id-for-futures-trade-history)
20. [Get Futures Trade History Download Link By Id](#20-get-futures-trade-history-download-link-by-id)
21. [Toggle BNB Burn On Futures Trade](#21-toggle-bnb-burn-on-futures-trade)
22. [Get BNB Burn Status](#22-get-bnb-burn-status)

---

## Deprecated / Removed Endpoints (No Longer in Current Docs)

The following endpoints from earlier API versions are no longer listed in the current Binance developer portal as of 2026-04-14. Their functionality has been consolidated into other endpoints (e.g., Change-Position-Mode and Change-Multi-Assets-Mode are now POST operations under the trade API section; Position ADL Quantile and User's Force Orders may have moved to the trade section):

- `POST /fapi/v1/positionSide/dual` -- Change Position Mode (now under Trade API)
- `POST /fapi/v1/multiAssetsMargin` -- Change Multi-Assets Mode (now under Trade API)
- `GET /fapi/v1/adlQuantile` -- Position ADL Quantile Estimation
- `GET /fapi/v1/forceOrders` -- User's Force Orders
- `GET /fapi/v1/rateLimit/order` -- Query User Rate Limit (see [#9 Query Order Rate Limit](#9-query-order-rate-limit) for current equivalent)

---

## 1. Futures Account Balance V3

**`GET /fapi/v3/balance`**

| Property | Value |
|----------|-------|
| Weight | 5 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response (Array)

| Field | Type | Description |
|-------|------|-------------|
| accountAlias | STRING | Unique account code |
| asset | STRING | Asset name (e.g. "USDT") |
| balance | STRING | Total wallet balance |
| crossWalletBalance | STRING | Crossed wallet balance |
| crossUnPnl | STRING | Unrealized PnL of crossed positions |
| availableBalance | STRING | Available balance for trading/withdrawal |
| maxWithdrawAmount | STRING | Maximum amount for transfer out |
| marginAvailable | BOOLEAN | Whether asset can be used as margin in Multi-Assets mode |
| updateTime | LONG | Last update timestamp (ms) |

### Example Response

```json
[
  {
    "accountAlias": "SgsR",
    "asset": "USDT",
    "balance": "122607.35137903",
    "crossWalletBalance": "23.72469206",
    "crossUnPnl": "0.00000000",
    "availableBalance": "23.72469206",
    "maxWithdrawAmount": "23.72469206",
    "marginAvailable": true,
    "updateTime": 1617939110373
  }
]
```

---

## 2. Futures Account Balance V2

**`GET /fapi/v2/balance`**

| Property | Value |
|----------|-------|
| Weight | 5 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response (Array)

| Field | Type | Description |
|-------|------|-------------|
| accountAlias | STRING | Unique account code |
| asset | STRING | Asset name (e.g. "USDT") |
| balance | STRING | Total wallet balance |
| crossWalletBalance | STRING | Crossed wallet balance |
| crossUnPnl | STRING | Unrealized PnL of crossed positions |
| availableBalance | STRING | Available balance for trading/withdrawal |
| maxWithdrawAmount | STRING | Maximum amount for transfer out |
| marginAvailable | BOOLEAN | Whether asset can be used as margin in Multi-Assets mode |
| updateTime | LONG | Last update timestamp (ms) |

### Example Response

```json
[
  {
    "accountAlias": "SgsR",
    "asset": "USDT",
    "balance": "122607.35137903",
    "crossWalletBalance": "23.72469206",
    "crossUnPnl": "0.00000000",
    "availableBalance": "23.72469206",
    "maxWithdrawAmount": "23.72469206",
    "marginAvailable": true,
    "updateTime": 1617939110373
  }
]
```

---

## 3. Account Information V3

**`GET /fapi/v3/account`**

| Property | Value |
|----------|-------|
| Weight | 5 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response -- Top-Level Fields

These fields differ based on margin mode:
- **Single-Asset Mode**: Values denominated in asset-specific terms
- **Multi-Assets Mode**: Values converted to USD equivalent

| Field | Type | Description |
|-------|------|-------------|
| totalInitialMargin | STRING | Total initial margin required |
| totalMaintMargin | STRING | Total maintenance margin required |
| totalWalletBalance | STRING | Total wallet balance |
| totalUnrealizedProfit | STRING | Total unrealized PnL |
| totalMarginBalance | STRING | Total margin balance |
| totalPositionInitialMargin | STRING | Initial margin for positions |
| totalOpenOrderInitialMargin | STRING | Initial margin for open orders |
| totalCrossWalletBalance | STRING | Cross wallet balance |
| totalCrossUnPnl | STRING | Unrealized PnL of crossed positions |
| availableBalance | STRING | Available balance |
| maxWithdrawAmount | STRING | Maximum withdrawal amount |

### Response -- Assets Array

| Field | Type | Description |
|-------|------|-------------|
| asset | STRING | Asset name (e.g. "USDT") |
| walletBalance | STRING | Wallet balance |
| unrealizedProfit | STRING | Unrealized PnL |
| marginBalance | STRING | Margin balance |
| maintMargin | STRING | Maintenance margin |
| initialMargin | STRING | Initial margin |
| positionInitialMargin | STRING | Position initial margin |
| openOrderInitialMargin | STRING | Open order initial margin |
| crossWalletBalance | STRING | Cross wallet balance |
| crossUnPnl | STRING | Cross unrealized PnL |
| availableBalance | STRING | Available balance |
| maxWithdrawAmount | STRING | Max withdrawal amount |
| updateTime | LONG | Last update timestamp (ms) |

### Response -- Positions Array

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair (e.g. "BTCUSDT") |
| positionSide | STRING | "BOTH" (one-way), "LONG" or "SHORT" (hedge mode) |
| positionAmt | STRING | Position quantity |
| unrealizedProfit | STRING | Position PnL |
| isolatedMargin | STRING | Isolated margin amount |
| notional | STRING | Position notional value |
| isolatedWallet | STRING | Isolated wallet balance |
| initialMargin | STRING | Initial margin required |
| maintMargin | STRING | Maintenance margin required |
| updateTime | LONG | Last position update (ms) |

### Notes
- One-way mode returns only "BOTH" position side
- Hedge mode returns separate "LONG" and "SHORT" positions
- Only positions with active holdings or open orders are included

---

## 4. Account Information V2

**`GET /fapi/v2/account`**

| Property | Value |
|----------|-------|
| Weight | 5 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response -- Top-Level Fields

| Field | Type | Description |
|-------|------|-------------|
| feeTier | INT | Account commission tier |
| feeBurn | BOOLEAN | Whether fee discount (BNB burn) is active |
| canTrade | BOOLEAN | Trading permission |
| canDeposit | BOOLEAN | Transfer-in permission |
| canWithdraw | BOOLEAN | Transfer-out permission |
| updateTime | LONG | Reserved field (ignore) |
| multiAssetsMargin | BOOLEAN | Whether multi-asset margin mode is enabled |
| tradeGroupId | INT | Trade group identifier |
| totalInitialMargin | STRING | Total initial margin required |
| totalMaintMargin | STRING | Total maintenance margin required |
| totalWalletBalance | STRING | Total wallet balance |
| totalUnrealizedProfit | STRING | Total unrealized PnL |
| totalMarginBalance | STRING | Total margin balance |
| totalCrossWalletBalance | STRING | Total cross wallet balance |
| totalCrossUnPnl | STRING | Cross unrealized PnL |
| availableBalance | STRING | Available balance |
| maxWithdrawAmount | STRING | Maximum withdrawal amount |

### Response -- Assets Array

| Field | Type | Description |
|-------|------|-------------|
| asset | STRING | Asset name |
| walletBalance | STRING | Wallet balance |
| unrealizedProfit | STRING | Unrealized PnL |
| marginBalance | STRING | Margin balance |
| maintMargin | STRING | Maintenance margin |
| initialMargin | STRING | Initial margin |
| positionInitialMargin | STRING | Position initial margin |
| openOrderInitialMargin | STRING | Open order initial margin |
| crossWalletBalance | STRING | Cross wallet balance |
| crossUnPnl | STRING | Cross unrealized PnL |
| availableBalance | STRING | Available balance (Multi-Assets mode: USD value) |
| maxWithdrawAmount | STRING | Max withdrawal amount |
| marginAvailable | BOOLEAN | Whether asset can be used as margin collateral |
| updateTime | LONG | Last update timestamp (ms) |

### Response -- Positions Array

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| initialMargin | STRING | Initial margin at current price |
| maintMargin | STRING | Maintenance margin threshold |
| unrealizedProfit | STRING | Open PnL |
| positionInitialMargin | STRING | Position-only margin |
| openOrderInitialMargin | STRING | Pending order margin |
| leverage | STRING | Current leverage multiplier |
| isolated | BOOLEAN | True = isolated margin; false = cross margin |
| entryPrice | STRING | Average fill price |
| maxNotional | STRING | Maximum notional value allowed |
| bidNotional | STRING | Bid side notional (reserved) |
| askNotional | STRING | Ask side notional (reserved) |
| positionSide | STRING | "BOTH" (one-way) or "LONG"/"SHORT" (hedge mode) |
| positionAmt | STRING | Position quantity |
| updateTime | LONG | Last update timestamp (ms) |

### Notes
- Single-Asset mode: top-level balance fields in asset denomination
- Multi-Assets mode: top-level balance fields converted to USD equivalent
- One-way mode returns only "BOTH" positions; hedge mode returns "LONG" and "SHORT"

---

## 5. Get Future Account Transaction History List

**Redirects to Wallet API**

This endpoint redirects to the universal transfer endpoint. See: `https://developers.binance.com/docs/wallet/asset/query-user-universal-transfer`

---

## 6. User Commission Rate

**`GET /fapi/v1/commissionRate`**

| Property | Value |
|----------|-------|
| Weight | 20 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | Yes | Trading pair symbol (e.g. "BTCUSDT") |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| makerCommissionRate | STRING | Maker fee rate (e.g. "0.0002" = 0.02%) |
| takerCommissionRate | STRING | Taker fee rate (e.g. "0.0004" = 0.04%) |
| rpiCommissionRate | STRING | RPI commission rate (e.g. "0.00005" = 0.005%) |

### Example Response

```json
{
  "symbol": "BTCUSDT",
  "makerCommissionRate": "0.0002",
  "takerCommissionRate": "0.0004",
  "rpiCommissionRate": "0.00005"
}
```

---

## 7. Query Account Configuration

**`GET /fapi/v1/accountConfig`**

| Property | Value |
|----------|-------|
| Weight | 5 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| feeTier | INT | Account commission tier |
| canTrade | BOOLEAN | Trading permission |
| canDeposit | BOOLEAN | Transfer-in permission |
| canWithdraw | BOOLEAN | Transfer-out permission |
| dualSidePosition | BOOLEAN | Hedge mode enabled (true = hedge, false = one-way) |
| updateTime | INT | Reserved field (ignore) |
| multiAssetsMargin | BOOLEAN | Multi-assets margin mode enabled |
| tradeGroupId | INT | Trade group identifier (-1 = none) |

### Example Response

```json
{
  "feeTier": 0,
  "canTrade": true,
  "canDeposit": true,
  "canWithdraw": true,
  "dualSidePosition": true,
  "updateTime": 0,
  "multiAssetsMargin": false,
  "tradeGroupId": -1
}
```

---

## 8. Query Symbol Configuration

**`GET /fapi/v1/symbolConfig`**

| Property | Value |
|----------|-------|
| Weight | 5 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | No | Trading pair to query (omit for all positions) |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response (Array)

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| marginType | STRING | Margin mode ("CROSSED" or "ISOLATED") |
| isAutoAddMargin | BOOLEAN | Auto-margin replenishment enabled |
| leverage | INT | Current leverage setting |
| maxNotionalValue | STRING | Maximum notional position value |

### Example Response

```json
[
  {
    "symbol": "BTCUSDT",
    "marginType": "CROSSED",
    "isAutoAddMargin": false,
    "leverage": 21,
    "maxNotionalValue": "1000000"
  }
]
```

---

## 9. Query Order Rate Limit

**`GET /fapi/v1/rateLimit/order`**

| Property | Value |
|----------|-------|
| Weight | 1 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response (Array)

| Field | Type | Description |
|-------|------|-------------|
| rateLimitType | STRING | Rate limit category (e.g. "ORDERS") |
| interval | STRING | Time interval unit (SECOND, MINUTE, etc.) |
| intervalNum | INT | Number of intervals |
| limit | INT | Maximum allowed within interval |

### Example Response

```json
[
  {
    "rateLimitType": "ORDERS",
    "interval": "SECOND",
    "intervalNum": 10,
    "limit": 10000
  },
  {
    "rateLimitType": "ORDERS",
    "interval": "MINUTE",
    "intervalNum": 1,
    "limit": 20000
  }
]
```

---

## 10. Notional and Leverage Brackets

**`GET /fapi/v1/leverageBracket`**

| Property | Value |
|----------|-------|
| Weight | 1 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | No | Trading pair (omit for all symbols) |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

When `symbol` is provided, returns a single object. When omitted, returns an array.

#### Top-Level

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| notionalCoef | DECIMAL | User symbol bracket multiplier (only appears when user's bracket is adjusted) |
| brackets | ARRAY | Array of bracket objects |

#### Bracket Object

| Field | Type | Description |
|-------|------|-------------|
| bracket | INT | Bracket level number |
| initialLeverage | INT | Max initial leverage for this bracket |
| notionalCap | DECIMAL | Cap notional of this bracket |
| notionalFloor | DECIMAL | Floor notional threshold of this bracket |
| maintMarginRatio | DECIMAL | Maintenance margin ratio for this bracket |
| cum | DECIMAL | Cumulative auxiliary value for margin calculation |

### Example Response (with symbol)

```json
{
  "symbol": "BTCUSDT",
  "notionalCoef": 1.5,
  "brackets": [
    {
      "bracket": 1,
      "initialLeverage": 125,
      "notionalCap": 50000,
      "notionalFloor": 0,
      "maintMarginRatio": 0.004,
      "cum": 0.0
    },
    {
      "bracket": 2,
      "initialLeverage": 100,
      "notionalCap": 250000,
      "notionalFloor": 50000,
      "maintMarginRatio": 0.005,
      "cum": 50.0
    }
  ]
}
```

---

## 11. Get Current Multi-Assets Mode

**`GET /fapi/v1/multiAssetsMargin`**

| Property | Value |
|----------|-------|
| Weight | 30 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| multiAssetsMargin | BOOLEAN | `true` = Multi-Assets Mode; `false` = Single-Asset Mode |

### Example Response

```json
{
  "multiAssetsMargin": true
}
```

### Notes
- Applies universally to all symbols in the account

---

## 12. Get Current Position Mode

**`GET /fapi/v1/positionSide/dual`**

| Property | Value |
|----------|-------|
| Weight | 30 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| dualSidePosition | BOOLEAN | `true` = Hedge Mode (long/short); `false` = One-way Mode |

### Example Response

```json
{
  "dualSidePosition": true
}
```

### Notes
- Applies to all symbols in the account
- Hedge Mode allows simultaneous long and short positions per symbol
- One-way Mode allows only a single directional position per symbol

---

## 13. Get Income History

**`GET /fapi/v1/income`**

| Property | Value |
|----------|-------|
| Weight | 30 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | No | Trading pair filter |
| incomeType | STRING | No | Income type filter (see enum below). Omit for all types. |
| startTime | LONG | No | Inclusive start timestamp (ms) |
| endTime | LONG | No | Inclusive end timestamp (ms) |
| page | INT | No | Pagination page number |
| limit | INT | No | Records per page (default: 100, max: 1000) |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

#### incomeType Enum

| Value | Description |
|-------|-------------|
| TRANSFER | Account transfer |
| WELCOME_BONUS | Welcome bonus |
| REALIZED_PNL | Realized profit/loss |
| FUNDING_FEE | Funding fee |
| COMMISSION | Trading commission |
| INSURANCE_CLEAR | Insurance fund clearance |
| REFERRAL_KICKBACK | Referral kickback |
| COMMISSION_REBATE | Commission rebate |
| API_REBATE | API rebate |
| CONTEST_REWARD | Contest reward |
| CROSS_COLLATERAL_TRANSFER | Cross collateral transfer |
| OPTIONS_PREMIUM_FEE | Options premium fee |
| OPTIONS_SETTLE_PROFIT | Options settlement profit |
| INTERNAL_TRANSFER | Internal transfer |
| AUTO_EXCHANGE | Auto exchange |
| DELIVERED_SETTLEMENT | Delivered settlement |
| COIN_SWAP_DEPOSIT | Coin swap deposit |
| COIN_SWAP_WITHDRAW | Coin swap withdrawal |
| POSITION_LIMIT_INCREASE_FEE | Position limit increase fee |
| STRATEGY_UMFUTURES_TRANSFER | Strategy USDⓈ-M futures transfer |
| FEE_RETURN | Fee return |
| BFUSD_REWARD | BFUSD reward |

### Response (Array)

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| incomeType | STRING | Income category |
| income | STRING | Income amount |
| asset | STRING | Asset denomination |
| info | STRING | Additional context/details |
| time | LONG | Event timestamp (ms) |
| tranId | LONG | Transaction ID (unique per incomeType per user) |
| tradeId | STRING | Associated trade ID (if applicable) |

### Example Response

```json
[
  {
    "symbol": "BTCUSDT",
    "incomeType": "COMMISSION",
    "income": "-0.01000000",
    "asset": "USDT",
    "info": "",
    "time": 1570636800000,
    "tranId": 9689322392,
    "tradeId": "2059192"
  }
]
```

### Notes
- When neither `startTime` nor `endTime` is provided, returns most recent 7 days of data
- Maximum lookback window: 90 days
- Transaction ID uniqueness applies within the same income category per user

---

## 14. Futures Trading Quantitative Rules Indicators

**`GET /fapi/v1/apiTradingStatus`**

| Property | Value |
|----------|-------|
| Weight | 1 (with symbol), 10 (without symbol) |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | No | Trading pair filter |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| indicators | OBJECT | Indicators organized by symbol key |
| updateTime | LONG | Last update timestamp (ms) |

#### Indicator Object (per symbol)

| Field | Type | Description |
|-------|------|-------------|
| indicator | STRING | Indicator type (see enum below) |
| value | DECIMAL | Current indicator value |
| triggerValue | DECIMAL | Threshold that triggers quantitative rules |
| isLocked | BOOLEAN | Whether account/symbol is currently locked |
| plannedRecoverTime | LONG | When restrictions will be lifted (ms) |

#### Indicator Type Enum

| Value | Description |
|-------|-------------|
| UFR | Unfilled Ratio |
| IFER | IOC/FOK Expiration Ratio |
| GCR | GTC Cancellation Ratio |
| DR | Dust Ratio |
| TMV | Too Many Violations (account-level, under "ACCOUNT" key) |

### Example Response

```json
{
  "indicators": {
    "BTCUSDT": [
      {
        "isLocked": false,
        "plannedRecoverTime": 0,
        "indicator": "UFR",
        "value": 0.05,
        "triggerValue": 0.995
      },
      {
        "isLocked": false,
        "plannedRecoverTime": 0,
        "indicator": "IFER",
        "value": 0.0,
        "triggerValue": 0.99
      },
      {
        "isLocked": false,
        "plannedRecoverTime": 0,
        "indicator": "GCR",
        "value": 0.0,
        "triggerValue": 0.99
      },
      {
        "isLocked": false,
        "plannedRecoverTime": 0,
        "indicator": "DR",
        "value": 0.0,
        "triggerValue": 0.99
      }
    ],
    "ACCOUNT": [
      {
        "isLocked": false,
        "plannedRecoverTime": 0,
        "indicator": "TMV",
        "value": 0,
        "triggerValue": 5
      }
    ]
  },
  "updateTime": 1612345678000
}
```

---

## 15. Get Download Id For Futures Transaction History

**`GET /fapi/v1/income/asyn`**

| Property | Value |
|----------|-------|
| Weight | 1000 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| startTime | LONG | Yes | Start timestamp (ms) |
| endTime | LONG | Yes | End timestamp (ms) |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| avgCostTimestampOfLast30d | INT | Average time taken for data download in past 30 days (ms) |
| downloadId | STRING | Unique download request identifier |

### Example Response

```json
{
  "avgCostTimestampOfLast30d": 7241837,
  "downloadId": "546975389218332672"
}
```

### Restrictions
- **Rate limit**: 5 requests per month (shared between frontend download page and REST API)
- **Time range**: `endTime - startTime` must not exceed 1 year

---

## 16. Get Futures Transaction History Download Link By Id

**`GET /fapi/v1/income/asyn/id`**

| Property | Value |
|----------|-------|
| Weight | 10 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| downloadId | STRING | Yes | Download ID from endpoint #15 |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| downloadId | STRING | Download request identifier |
| status | STRING | "completed" or "processing" |
| url | STRING | Download URL (empty if still processing) |
| notified | BOOLEAN | Internal flag (ignore) |
| expirationTimestamp | LONG | When download link expires (ms); -1 if processing |
| isExpired | BOOLEAN/NULL | Link expiration status |

### Example Response (Completed)

```json
{
  "downloadId": "545923594199212032",
  "status": "completed",
  "url": "www.binance.com",
  "notified": true,
  "expirationTimestamp": 1645009771000,
  "isExpired": null
}
```

### Example Response (Processing)

```json
{
  "downloadId": "545923594199212032",
  "status": "processing",
  "url": "",
  "notified": false,
  "expirationTimestamp": -1,
  "isExpired": null
}
```

### Notes
- Download links expire after 24 hours

---

## 17. Get Download Id For Futures Order History

**`GET /fapi/v1/order/asyn`**

| Property | Value |
|----------|-------|
| Weight | 1000 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| startTime | LONG | Yes | Start timestamp (ms) |
| endTime | LONG | Yes | End timestamp (ms) |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| avgCostTimestampOfLast30d | INT | Average time for download in past 30 days (ms) |
| downloadId | STRING | Unique download request identifier |

### Example Response

```json
{
  "avgCostTimestampOfLast30d": 7241837,
  "downloadId": "546975389218332672"
}
```

### Restrictions
- **Rate limit**: 10 requests per month (shared between frontend download page and REST API)
- **Time range**: `endTime - startTime` must not exceed 1 year

---

## 18. Get Futures Order History Download Link By Id

**`GET /fapi/v1/order/asyn/id`**

| Property | Value |
|----------|-------|
| Weight | 10 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| downloadId | STRING | Yes | Download ID from endpoint #17 |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| downloadId | STRING | Download request identifier |
| status | STRING | "completed" or "processing" |
| url | STRING | Download URL (empty if processing) |
| notified | BOOLEAN | Internal flag (ignore) |
| expirationTimestamp | LONG | Link expiry timestamp (ms); -1 if processing |
| isExpired | BOOLEAN/NULL | Expiration status |

### Example Response (Completed)

```json
{
  "downloadId": "545923594199212032",
  "status": "completed",
  "url": "www.binance.com",
  "notified": true,
  "expirationTimestamp": 1645009771000,
  "isExpired": null
}
```

### Notes
- Download links expire after 24 hours

---

## 19. Get Download Id For Futures Trade History

**`GET /fapi/v1/trade/asyn`**

| Property | Value |
|----------|-------|
| Weight | 1000 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| startTime | LONG | Yes | Start timestamp (ms) |
| endTime | LONG | Yes | End timestamp (ms) |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| avgCostTimestampOfLast30d | INT | Average download time in past 30 days (ms) |
| downloadId | STRING | Unique download request identifier |

### Example Response

```json
{
  "avgCostTimestampOfLast30d": 7241837,
  "downloadId": "546975389218332672"
}
```

### Restrictions
- **Rate limit**: 5 requests per month (shared between frontend download page and REST API)
- **Time range**: `endTime - startTime` must not exceed 1 year

---

## 20. Get Futures Trade History Download Link By Id

**`GET /fapi/v1/trade/asyn/id`**

| Property | Value |
|----------|-------|
| Weight | 10 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| downloadId | STRING | Yes | Download ID from endpoint #19 |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| downloadId | STRING | Download request identifier |
| status | STRING | "completed" or "processing" |
| url | STRING | Download URL (empty if processing) |
| notified | BOOLEAN | Internal flag (ignore) |
| expirationTimestamp | LONG | Link expiry timestamp (ms); -1 if processing |
| isExpired | BOOLEAN/NULL | Expiration status |

### Notes
- Download links expire after 24 hours

---

## 21. Toggle BNB Burn On Futures Trade

**`POST /fapi/v1/feeBurn`**

| Property | Value |
|----------|-------|
| Weight | 1 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| feeBurn | STRING | Yes | "true" to activate BNB fee discount; "false" to deactivate |
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| code | INT | Response code (200 = success) |
| msg | STRING | Response message |

### Example Response

```json
{
  "code": 200,
  "msg": "success"
}
```

### Notes
- Applies universally to all symbols in the account
- When enabled, BNB is used to pay trading fees at a discounted rate

---

## 22. Get BNB Burn Status

**`GET /fapi/v1/feeBurn`**

| Property | Value |
|----------|-------|
| Weight | 30 |
| Security | USER_DATA |

### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | No | Request validity window (ms) |
| timestamp | LONG | Yes | Current timestamp (ms) |

### Response

| Field | Type | Description |
|-------|------|-------------|
| feeBurn | BOOLEAN | `true` = BNB fee discount active; `false` = inactive |

### Example Response

```json
{
  "feeBurn": true
}
```

---

## Endpoint Summary Table

| # | Method | Endpoint | Weight | Description |
|---|--------|----------|--------|-------------|
| 1 | GET | `/fapi/v3/balance` | 5 | Account balance (V3) |
| 2 | GET | `/fapi/v2/balance` | 5 | Account balance (V2) |
| 3 | GET | `/fapi/v3/account` | 5 | Account info (V3) |
| 4 | GET | `/fapi/v2/account` | 5 | Account info (V2) |
| 5 | -- | -- | -- | Redirects to wallet universal transfer |
| 6 | GET | `/fapi/v1/commissionRate` | 20 | Commission rate for a symbol |
| 7 | GET | `/fapi/v1/accountConfig` | 5 | Account configuration |
| 8 | GET | `/fapi/v1/symbolConfig` | 5 | Symbol configuration |
| 9 | GET | `/fapi/v1/rateLimit/order` | 1 | Order rate limits |
| 10 | GET | `/fapi/v1/leverageBracket` | 1 | Leverage brackets by notional |
| 11 | GET | `/fapi/v1/multiAssetsMargin` | 30 | Multi-assets mode status |
| 12 | GET | `/fapi/v1/positionSide/dual` | 30 | Position mode (hedge/one-way) |
| 13 | GET | `/fapi/v1/income` | 30 | Income history |
| 14 | GET | `/fapi/v1/apiTradingStatus` | 1/10 | Quantitative rules indicators |
| 15 | GET | `/fapi/v1/income/asyn` | 1000 | Download ID for transaction history |
| 16 | GET | `/fapi/v1/income/asyn/id` | 10 | Transaction history download link |
| 17 | GET | `/fapi/v1/order/asyn` | 1000 | Download ID for order history |
| 18 | GET | `/fapi/v1/order/asyn/id` | 10 | Order history download link |
| 19 | GET | `/fapi/v1/trade/asyn` | 1000 | Download ID for trade history |
| 20 | GET | `/fapi/v1/trade/asyn/id` | 10 | Trade history download link |
| 21 | POST | `/fapi/v1/feeBurn` | 1 | Toggle BNB burn for fee discount |
| 22 | GET | `/fapi/v1/feeBurn` | 30 | Get BNB burn status |
