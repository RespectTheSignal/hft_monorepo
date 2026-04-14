# Binance USDS-M Futures - Market Data REST API

Base URL: `https://fapi.binance.com`

Source: https://developers.binance.com/docs/derivatives/usds-margined-futures/market-data/rest-api/

---

## 1. Test Connectivity

- **Method:** `GET`
- **Path:** `/fapi/v1/ping`
- **Description:** Test connectivity to the REST API.
- **Weight:** 1

### Parameters

None.

### Response

```json
{}
```

Empty JSON object confirming connectivity.

---

## 2. Check Server Time

- **Method:** `GET`
- **Path:** `/fapi/v1/time`
- **Description:** Test connectivity to the REST API and get the current server time.
- **Weight:** 1

### Parameters

None.

### Response

```json
{
  "serverTime": 1499827319559
}
```

| Field | Type | Description |
|-------|------|-------------|
| `serverTime` | LONG | Current server time in milliseconds (Unix timestamp) |

---

## 3. Exchange Information

- **Method:** `GET`
- **Path:** `/fapi/v1/exchangeInfo`
- **Description:** Current exchange trading rules and symbol information.
- **Weight:** 1

### Parameters

None.

### Response

| Field | Type | Description |
|-------|------|-------------|
| `timezone` | STRING | Exchange timezone (UTC) |
| `serverTime` | LONG | Server timestamp (for reference only) |
| `rateLimits` | ARRAY | Rate limiting configuration |
| `exchangeFilters` | ARRAY | Exchange-level filter rules |
| `assets` | ARRAY | Available trading assets |
| `symbols` | ARRAY | Trading pair specifications |

#### rateLimits Object

| Field | Type | Description |
|-------|------|-------------|
| `rateLimitType` | STRING | `REQUEST_WEIGHT` or `ORDERS` |
| `interval` | STRING | Time interval (e.g. `MINUTE`) |
| `intervalNum` | INT | Number of intervals |
| `limit` | INT | Request/order limit |

#### assets Object

| Field | Type | Description |
|-------|------|-------------|
| `asset` | STRING | Asset symbol (e.g. `USDT`, `BTC`) |
| `marginAvailable` | BOOLEAN | Whether usable as margin in Multi-Assets mode |
| `autoAssetExchange` | STRING | Auto-exchange threshold value |

#### symbols Object

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair identifier (e.g. `BTCUSDT`) |
| `pair` | STRING | Pair notation |
| `contractType` | STRING | `PERPETUAL`, `CURRENT_QUARTER`, `NEXT_QUARTER`, etc. |
| `deliveryDate` | LONG | Delivery timestamp (ms) |
| `onboardDate` | LONG | Listing timestamp (ms) |
| `status` | STRING | `TRADING` or other status |
| `baseAsset` | STRING | Base asset (e.g. `BTC`) |
| `quoteAsset` | STRING | Quote asset (e.g. `USDT`) |
| `marginAsset` | STRING | Margin denomination |
| `pricePrecision` | INT | Price decimal places |
| `quantityPrecision` | INT | Quantity decimal places |
| `baseAssetPrecision` | INT | Base asset precision |
| `quotePrecision` | INT | Quote asset precision |
| `underlyingType` | STRING | `COIN` or similar |
| `underlyingSubType` | ARRAY | Sub-categories (e.g. `["STORAGE"]`) |
| `triggerProtect` | STRING | Algo order price protection threshold |
| `liquidationFee` | STRING | Fee rate for liquidations |
| `marketTakeBound` | STRING | Max price variance from mark price for market orders |
| `filters` | ARRAY | Trading rule filters |
| `orderTypes` | ARRAY | Supported order types |
| `timeInForce` | ARRAY | Supported time-in-force options |

#### Filter Types

| Filter | Fields | Description |
|--------|--------|-------------|
| `PRICE_FILTER` | `minPrice`, `maxPrice`, `tickSize` | Valid price range and tick |
| `LOT_SIZE` | `minQty`, `maxQty`, `stepSize` | Valid quantity range and step |
| `MARKET_LOT_SIZE` | `minQty`, `maxQty`, `stepSize` | Market order quantity limits |
| `MAX_NUM_ORDERS` | `limit` | Max open orders |
| `MAX_NUM_ALGO_ORDERS` | `limit` | Max open algo orders |
| `MIN_NOTIONAL` | `notional` | Minimum notional value |
| `PERCENT_PRICE` | `multiplierUp`, `multiplierDown`, `multiplierDecimal` | Price percentage filter |

---

## 4. Order Book

- **Method:** `GET`
- **Path:** `/fapi/v1/depth`
- **Description:** Get order book depth data.
- **Weight:** Varies by limit (see below)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `limit` | INT | NO | Default 500. Valid: `[5, 10, 20, 50, 100, 500, 1000]` |

### Weight by Limit

| Limit | Weight |
|-------|--------|
| 5, 10, 20, 50 | 2 |
| 100 | 5 |
| 500 | 10 |
| 1000 | 20 |

### Response

```json
{
  "lastUpdateId": 1027024,
  "E": 1589436922972,
  "T": 1589436922959,
  "bids": [
    ["4.00000000", "431.00000000"]
  ],
  "asks": [
    ["4.00000200", "12.00000000"]
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `lastUpdateId` | LONG | Last update ID for the order book |
| `E` | LONG | Message output time (ms) |
| `T` | LONG | Transaction time (ms) |
| `bids` | ARRAY | Bid levels: `[price, quantity]` (strings) |
| `asks` | ARRAY | Ask levels: `[price, quantity]` (strings) |

> Note: Retail Price Improvement (RPI) orders are excluded from this response.

---

## 5. RPI Order Book

- **Method:** `GET`
- **Path:** `/fapi/v1/rpiDepth`
- **Description:** Order book data including Retail Price Improvement (RPI) orders.
- **Weight:** 20

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `limit` | INT | NO | Default 1000. Valid: `[1000]` |

### Response

```json
{
  "lastUpdateId": 1027024,
  "E": 1589436922972,
  "T": 1589436922959,
  "bids": [
    ["4.00000000", "431.00000000"]
  ],
  "asks": [
    ["4.00000200", "12.00000000"]
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `lastUpdateId` | LONG | Last update ID |
| `E` | LONG | Message output time (ms) |
| `T` | LONG | Transaction time (ms) |
| `bids` | ARRAY | Bid levels: `[price, quantity]` (strings). RPI orders included and aggregated. |
| `asks` | ARRAY | Ask levels: `[price, quantity]` (strings). RPI orders included and aggregated. |

> Note: Crossed price levels remain hidden from view.

---

## 6. Recent Trades List

- **Method:** `GET`
- **Path:** `/fapi/v1/trades`
- **Description:** Get recent trades filled in the order book (excludes insurance fund and ADL trades).
- **Weight:** 5

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `limit` | INT | NO | Default 500; max 1000 |

### Response

```json
[
  {
    "id": 28457,
    "price": "4.00000100",
    "qty": "12.00000000",
    "quoteQty": "48.00",
    "time": 1499865549590,
    "isBuyerMaker": true,
    "isRPITrade": true
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | LONG | Trade ID |
| `price` | STRING | Trade price |
| `qty` | STRING | Trade quantity |
| `quoteQty` | STRING | Quote asset quantity |
| `time` | LONG | Trade timestamp (ms) |
| `isBuyerMaker` | BOOLEAN | Whether buyer is the maker |
| `isRPITrade` | BOOLEAN | Whether trade is an RPI trade |

---

## 7. Old Trades Lookup

- **Method:** `GET`
- **Path:** `/fapi/v1/historicalTrades`
- **Description:** Get older market trades. Only supports data from within the last one month.
- **Weight:** 20

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `limit` | INT | NO | Default 100; max 500 |
| `fromId` | LONG | NO | Trade ID to retrieve from. Defaults to most recent trades. |

### Response

```json
[
  {
    "id": 28457,
    "price": "4.00000100",
    "qty": "12.00000000",
    "quoteQty": "48.00",
    "time": 1499865549590,
    "isBuyerMaker": true,
    "isRPITrade": true
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | LONG | Trade ID |
| `price` | STRING | Trade price |
| `qty` | STRING | Trade quantity |
| `quoteQty` | STRING | Quote asset quantity |
| `time` | LONG | Trade timestamp (ms) |
| `isBuyerMaker` | BOOLEAN | Whether buyer is the maker |
| `isRPITrade` | BOOLEAN | Whether trade is an RPI trade |

> Note: Only market trades returned; insurance fund and ADL trades excluded.

---

## 8. Compressed/Aggregate Trades List

- **Method:** `GET`
- **Path:** `/fapi/v1/aggTrades`
- **Description:** Get compressed, aggregate trades. Trades that fill at the same time, from the same order, at the same price will be aggregated.
- **Weight:** 20

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `fromId` | LONG | NO | Starting aggregate trade ID (inclusive) |
| `startTime` | LONG | NO | Start timestamp (ms, inclusive) |
| `endTime` | LONG | NO | End timestamp (ms, inclusive) |
| `limit` | INT | NO | Default 500; max 1000 |

### Constraints

- Historical data available for up to one year.
- When both `startTime` and `endTime` are provided, the duration must not exceed one hour.
- Default behavior returns most recent trades when no filtering parameters are sent.
- Only market trades are aggregated; insurance fund and ADL trades excluded.

### Response

```json
[
  {
    "a": 26129,
    "p": "0.01633102",
    "q": "4.70443515",
    "f": 27781,
    "l": 27781,
    "T": 1498793709153,
    "m": true
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `a` | LONG | Aggregate trade ID |
| `p` | STRING | Price |
| `q` | STRING | Quantity |
| `f` | LONG | First trade ID in aggregate |
| `l` | LONG | Last trade ID in aggregate |
| `T` | LONG | Timestamp (ms) |
| `m` | BOOLEAN | Whether buyer was the maker |

---

## 9. Kline/Candlestick Data

- **Method:** `GET`
- **Path:** `/fapi/v1/klines`
- **Description:** Kline/candlestick bars for a symbol. Klines are uniquely identified by their open time.
- **Weight:** Varies by limit (see below)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `interval` | ENUM | YES | Kline interval (e.g. `1m`,`3m`,`5m`,`15m`,`30m`,`1h`,`2h`,`4h`,`6h`,`8h`,`12h`,`1d`,`3d`,`1w`,`1M`) |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |
| `limit` | INT | NO | Default 500; max 1500 |

> If startTime and endTime are not sent, the most recent klines are returned.

### Weight by Limit

| Limit Range | Weight |
|-------------|--------|
| [1, 100) | 1 |
| [100, 500) | 2 |
| [500, 1000] | 5 |
| > 1000 | 10 |

### Response

Array of arrays. Each inner array:

| Index | Type | Description |
|-------|------|-------------|
| 0 | LONG | Open time (ms) |
| 1 | STRING | Open price |
| 2 | STRING | High price |
| 3 | STRING | Low price |
| 4 | STRING | Close price |
| 5 | STRING | Volume |
| 6 | LONG | Close time (ms) |
| 7 | STRING | Quote asset volume |
| 8 | INT | Number of trades |
| 9 | STRING | Taker buy base asset volume |
| 10 | STRING | Taker buy quote asset volume |
| 11 | STRING | Ignore |

---

## 10. Continuous Contract Kline/Candlestick Data

- **Method:** `GET`
- **Path:** `/fapi/v1/continuousKlines`
- **Description:** Kline/candlestick bars for a specific contract type. Klines are uniquely identified by their open time.
- **Weight:** Varies by limit (same as Kline endpoint)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `pair` | STRING | YES | Trading pair (e.g. `BTCUSDT`) |
| `contractType` | ENUM | YES | `PERPETUAL`, `CURRENT_QUARTER`, `NEXT_QUARTER`, `TRADIFI_PERPETUAL` |
| `interval` | ENUM | YES | Kline interval |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |
| `limit` | INT | NO | Default 500; max 1500 |

> If startTime and endTime are not sent, the most recent klines are returned.

### Weight by Limit

| Limit Range | Weight |
|-------------|--------|
| [1, 100) | 1 |
| [100, 500) | 2 |
| [500, 1000] | 5 |
| > 1000 | 10 |

### Response

Same format as Kline/Candlestick Data (Section 9).

---

## 11. Index Price Kline/Candlestick Data

- **Method:** `GET`
- **Path:** `/fapi/v1/indexPriceKlines`
- **Description:** Kline/candlestick bars for index price of a pair.
- **Weight:** Varies by limit (same as Kline endpoint)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `pair` | STRING | YES | Trading pair symbol |
| `interval` | ENUM | YES | Kline interval |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |
| `limit` | INT | NO | Default 500; max 1500 |

> If startTime and endTime are not sent, the most recent klines are returned.

### Weight by Limit

| Limit Range | Weight |
|-------------|--------|
| [1, 100) | 1 |
| [100, 500) | 2 |
| [500, 1000] | 5 |
| > 1000 | 10 |

### Response

Array of arrays. Each inner array:

| Index | Type | Description |
|-------|------|-------------|
| 0 | LONG | Open time (ms) |
| 1 | STRING | Open price |
| 2 | STRING | High price |
| 3 | STRING | Low price |
| 4 | STRING | Close price (latest price for incomplete kline) |
| 5 | STRING | Ignore |
| 6 | LONG | Close time (ms) |
| 7-11 | - | Ignore |

---

## 12. Mark Price Kline/Candlestick Data

- **Method:** `GET`
- **Path:** `/fapi/v1/markPriceKlines`
- **Description:** Kline/candlestick bars for mark price of a symbol.
- **Weight:** Varies by limit (same as Kline endpoint)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `interval` | ENUM | YES | Kline interval |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |
| `limit` | INT | NO | Default 500; max 1500 |

> If startTime and endTime are not sent, the most recent klines are returned.

### Weight by Limit

| Limit Range | Weight |
|-------------|--------|
| [1, 100) | 1 |
| [100, 500) | 2 |
| [500, 1000] | 5 |
| > 1000 | 10 |

### Response

Array of arrays. Each inner array:

| Index | Type | Description |
|-------|------|-------------|
| 0 | LONG | Open time (ms) |
| 1 | STRING | Open (mark price) |
| 2 | STRING | High (mark price) |
| 3 | STRING | Low (mark price) |
| 4 | STRING | Close (latest mark price) |
| 5 | STRING | Ignore |
| 6 | LONG | Close time (ms) |
| 7-11 | - | Ignore |

---

## 13. Premium Index Kline Data

- **Method:** `GET`
- **Path:** `/fapi/v1/premiumIndexKlines`
- **Description:** Premium index kline/candlestick bars for a symbol. Klines are uniquely identified by their open time.
- **Weight:** Varies by limit (same as Kline endpoint)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `interval` | ENUM | YES | Kline interval |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |
| `limit` | INT | NO | Default 500; max 1500 |

> If startTime and endTime are not sent, the most recent klines are returned.

### Weight by Limit

| Limit Range | Weight |
|-------------|--------|
| [1, 100) | 1 |
| [100, 500) | 2 |
| [500, 1000] | 5 |
| > 1000 | 10 |

### Response

```json
[
  [
    1691603820000,
    "-0.00042931",
    "-0.00023641",
    "-0.00059406",
    "-0.00043659",
    "0",
    1691603879999,
    "0",
    12,
    "0",
    "0",
    "0"
  ]
]
```

| Index | Type | Description |
|-------|------|-------------|
| 0 | LONG | Open time (ms) |
| 1 | STRING | Open (premium index) |
| 2 | STRING | High |
| 3 | STRING | Low |
| 4 | STRING | Close |
| 5 | STRING | Ignore |
| 6 | LONG | Close time (ms) |
| 7 | STRING | Ignore |
| 8 | INT | Ignore |
| 9 | STRING | Ignore |
| 10 | STRING | Ignore |
| 11 | STRING | Ignore |

---

## 14. Mark Price

- **Method:** `GET`
- **Path:** `/fapi/v1/premiumIndex`
- **Description:** Get mark price and funding rate information.
- **Weight:** 1 (with symbol), 10 (without symbol)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | Trading pair symbol. When omitted, returns data for all symbols. |

### Response (single symbol)

```json
{
  "symbol": "BTCUSDT",
  "markPrice": "11793.63104562",
  "indexPrice": "11781.80495970",
  "estimatedSettlePrice": "11781.16138815",
  "lastFundingRate": "0.00038246",
  "interestRate": "0.00010000",
  "nextFundingTime": 1597392000000,
  "time": 1597370495002
}
```

When symbol is omitted, response is an array of objects with the same structure.

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair identifier |
| `markPrice` | STRING | Current mark price |
| `indexPrice` | STRING | Current index price |
| `estimatedSettlePrice` | STRING | Estimated settlement price (relevant only in the last hour before settlement) |
| `lastFundingRate` | STRING | Most recent funding rate |
| `interestRate` | STRING | Interest rate |
| `nextFundingTime` | LONG | Unix timestamp of next funding event (ms) |
| `time` | LONG | Response timestamp (ms) |

---

## 15. Get Funding Rate History

- **Method:** `GET`
- **Path:** `/fapi/v1/fundingRate`
- **Description:** Get funding rate history.
- **Weight:** Shares 500/5min/IP rate limit with `GET /fapi/v1/fundingInfo`

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | Trading pair symbol |
| `startTime` | LONG | NO | Timestamp (ms) to get funding rate from (inclusive) |
| `endTime` | LONG | NO | Timestamp (ms) to get funding rate until (inclusive) |
| `limit` | INT | NO | Default 100; max 1000 |

### Query Behavior

- Without `startTime` and `endTime`: returns most recent 200 records.
- If data volume exceeds `limit`: returns from `startTime` + `limit` quantity.
- Results are ordered in ascending sequence.

### Response

```json
[
  {
    "symbol": "BTCUSDT",
    "fundingRate": "-0.03750000",
    "fundingTime": 1570608000000,
    "markPrice": "34287.54619963"
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `fundingRate` | STRING | Funding rate value |
| `fundingTime` | LONG | Funding timestamp (ms) |
| `markPrice` | STRING | Mark price associated with the funding fee charge |

---

## 16. Get Funding Rate Info

- **Method:** `GET`
- **Path:** `/fapi/v1/fundingInfo`
- **Description:** Query funding rate info for symbols that had FundingRateCap / FundingRateFloor / fundingIntervalHours adjustment.
- **Weight:** 0 (shares 500/5min/IP rate limit with `GET /fapi/v1/fundingRate`)

### Parameters

None.

### Response

```json
[
  {
    "symbol": "BLZUSDT",
    "adjustedFundingRateCap": "0.02500000",
    "adjustedFundingRateFloor": "-0.02500000",
    "fundingIntervalHours": 8,
    "disclaimer": false
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `adjustedFundingRateCap` | STRING | Maximum funding rate cap |
| `adjustedFundingRateFloor` | STRING | Minimum funding rate floor |
| `fundingIntervalHours` | INT | Funding rate interval duration (hours) |
| `disclaimer` | BOOLEAN | Disclaimer flag (ignore) |

---

## 17. 24hr Ticker Price Change Statistics

- **Method:** `GET`
- **Path:** `/fapi/v1/ticker/24hr`
- **Description:** 24 hour rolling window price change statistics.
- **Weight:** 1 (with symbol), 40 (without symbol)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | Trading pair symbol. When omitted, returns data for all symbols (array). |

> Careful when accessing this with no symbol due to the much higher weight cost.

### Response

```json
{
  "symbol": "BTCUSDT",
  "priceChange": "-94.99999800",
  "priceChangePercent": "-95.960",
  "weightedAvgPrice": "0.29628482",
  "lastPrice": "4.00000200",
  "lastQty": "200.00000000",
  "openPrice": "99.00000000",
  "highPrice": "100.00000000",
  "lowPrice": "0.10000000",
  "volume": "8913.30000000",
  "quoteVolume": "15.30000000",
  "openTime": 1499783499040,
  "closeTime": 1499869899040,
  "firstId": 28385,
  "lastId": 28460,
  "count": 76
}
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair identifier |
| `priceChange` | STRING | Price change amount |
| `priceChangePercent` | STRING | Price change percentage |
| `weightedAvgPrice` | STRING | Weighted average price |
| `lastPrice` | STRING | Most recent trade price |
| `lastQty` | STRING | Most recent trade quantity |
| `openPrice` | STRING | 24hr opening price |
| `highPrice` | STRING | 24hr highest price |
| `lowPrice` | STRING | 24hr lowest price |
| `volume` | STRING | 24hr trading volume |
| `quoteVolume` | STRING | 24hr quote asset volume |
| `openTime` | LONG | 24hr window open timestamp (ms) |
| `closeTime` | LONG | Current timestamp (ms) |
| `firstId` | LONG | First trade ID in the window |
| `lastId` | LONG | Last trade ID in the window |
| `count` | INT | Total trades in the 24hr window |

---

## 18. Symbol Price Ticker (Deprecated)

- **Method:** `GET`
- **Path:** `/fapi/v1/ticker/price`
- **Description:** Latest price for a symbol or symbols. **DEPRECATED** -- use Symbol Price Ticker V2 instead.
- **Weight:** 1 (with symbol), 2 (without symbol)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | When omitted, returns prices for all symbols (array). |

### Response (single symbol)

```json
{
  "symbol": "BTCUSDT",
  "price": "6000.01",
  "time": 1589437530011
}
```

When symbol omitted, response is an array of objects with the same structure.

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `price` | STRING | Current price |
| `time` | LONG | Transaction timestamp (ms) |

---

## 19. Symbol Price Ticker V2

- **Method:** `GET`
- **Path:** `/fapi/v2/ticker/price`
- **Description:** Latest price for a symbol or symbols (replacement for deprecated v1).
- **Weight:** 1 (with symbol), 2 (without symbol)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | When omitted, returns prices for all symbols (array). |

> Note: The `X-MBX-USED-WEIGHT-1M` response header is not accurate from this endpoint; please ignore.

### Response (single symbol)

```json
{
  "symbol": "BTCUSDT",
  "price": "6000.01",
  "time": 1589437530011
}
```

When symbol omitted, response is an array of objects with the same structure.

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `price` | STRING | Current price |
| `time` | LONG | Transaction timestamp (ms) |

---

## 20. Symbol Order Book Ticker

- **Method:** `GET`
- **Path:** `/fapi/v1/ticker/bookTicker`
- **Description:** Best price/qty on the order book for a symbol or symbols.
- **Weight:** 2 (with symbol), 5 (without symbol)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | When omitted, returns bookTickers for all symbols (array). |

> Note: The `X-MBX-USED-WEIGHT-1M` response header is inaccurate for this endpoint and should be disregarded.

### Response (single symbol)

```json
{
  "symbol": "BTCUSDT",
  "bidPrice": "4.00000000",
  "bidQty": "431.00000000",
  "askPrice": "4.00000200",
  "askQty": "9.00000000",
  "time": 1589437530011
}
```

When symbol omitted, response is an array of objects with the same structure.

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `bidPrice` | STRING | Best bid price |
| `bidQty` | STRING | Best bid quantity |
| `askPrice` | STRING | Best ask price |
| `askQty` | STRING | Best ask quantity |
| `time` | LONG | Transaction timestamp (ms) |

> Note: RPI orders are excluded from the response.

---

## 21. Query Delivery Price

- **Method:** `GET`
- **Path:** `/futures/data/delivery-price`
- **Description:** Get delivery price history for quarterly contracts.
- **Weight:** 0

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `pair` | STRING | YES | Trading pair (e.g. `BTCUSDT`) |

### Response

```json
[
  {
    "deliveryTime": 1695945600000,
    "deliveryPrice": "27103.00000000"
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `deliveryTime` | LONG | Settlement timestamp (ms) |
| `deliveryPrice` | STRING | Settlement price |

---

## 22. Open Interest

- **Method:** `GET`
- **Path:** `/fapi/v1/openInterest`
- **Description:** Get present open interest of a specific symbol.
- **Weight:** 1

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |

### Response

```json
{
  "openInterest": "10659.509",
  "symbol": "BTCUSDT",
  "time": 1589437530011
}
```

| Field | Type | Description |
|-------|------|-------------|
| `openInterest` | STRING | Current open interest value |
| `symbol` | STRING | Trading pair symbol |
| `time` | LONG | Timestamp (ms) |

---

## 23. Open Interest Statistics

- **Method:** `GET`
- **Path:** `/futures/data/openInterestHist`
- **Description:** Historical open interest statistics.
- **Weight:** 0 (rate limit: 1000 requests/5min per IP)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `period` | ENUM | YES | `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `12h`, `1d` |
| `limit` | LONG | NO | Default 30; max 500 |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |

> Only the data of the latest 1 month is available. If startTime and endTime are not sent, the most recent data is returned.

### Response

```json
[
  {
    "symbol": "BTCUSDT",
    "sumOpenInterest": "10659.50900000",
    "sumOpenInterestValue": "574024861.60000000",
    "CMCCirculatingSupply": "19500000.00000000",
    "timestamp": 1589437530011
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `sumOpenInterest` | STRING | Aggregate open interest volume |
| `sumOpenInterestValue` | STRING | Total open interest value |
| `CMCCirculatingSupply` | STRING | CoinMarketCap circulating supply |
| `timestamp` | LONG | Timestamp (ms) |

---

## 24. Top Trader Long/Short Ratio (Positions)

- **Method:** `GET`
- **Path:** `/futures/data/topLongShortPositionRatio`
- **Description:** Long/short position ratio of top traders (top 20% by margin).
- **Weight:** 0 (rate limit: 1000 requests/5min per IP)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `period` | ENUM | YES | `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `12h`, `1d` |
| `limit` | LONG | NO | Default 30; max 500 |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |

> Only the data of the latest 30 days is available. If startTime and endTime are not sent, the most recent data is returned.

### Response

```json
[
  {
    "symbol": "BTCUSDT",
    "longShortRatio": "1.8105",
    "longAccount": "0.6442",
    "shortAccount": "0.3558",
    "timestamp": 1589437530011
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `longShortRatio` | STRING | Long/short position ratio of top traders |
| `longAccount` | STRING | Proportion of long positions among top traders |
| `shortAccount` | STRING | Proportion of short positions among top traders |
| `timestamp` | LONG | Timestamp (ms) |

---

## 25. Top Trader Long/Short Ratio (Accounts)

- **Method:** `GET`
- **Path:** `/futures/data/topLongShortAccountRatio`
- **Description:** Long/short account ratio of top traders (top 20% users with highest margin balance). Each account is counted once only.
- **Weight:** 0 (rate limit: 1000 requests/5min per IP)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `period` | ENUM | YES | `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `12h`, `1d` |
| `limit` | LONG | NO | Default 30; max 500 |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |

> Only the data of the latest 30 days is available. If startTime and endTime are not sent, the most recent data is returned.

### Response

```json
[
  {
    "symbol": "BTCUSDT",
    "longShortRatio": "1.8105",
    "longAccount": "0.6442",
    "shortAccount": "0.3558",
    "timestamp": 1589437530011
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `longShortRatio` | STRING | Ratio of long to short accounts among top 20% traders |
| `longAccount` | STRING | Percentage of accounts with net long positions |
| `shortAccount` | STRING | Percentage of accounts with net short positions |
| `timestamp` | LONG | Timestamp (ms) |

---

## 26. Long/Short Ratio

- **Method:** `GET`
- **Path:** `/futures/data/globalLongShortAccountRatio`
- **Description:** Long/short ratio across all traders (global account ratio).
- **Weight:** 0 (rate limit: 1000 requests/5min per IP)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `period` | ENUM | YES | `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `12h`, `1d` |
| `limit` | LONG | NO | Default 30; max 500 |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |

> Only the data of the latest 30 days is available. If startTime and endTime are not sent, the most recent data is returned.

### Response

```json
[
  {
    "symbol": "BTCUSDT",
    "longShortRatio": "1.8105",
    "longAccount": "0.6442",
    "shortAccount": "0.3558",
    "timestamp": 1589437530011
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `longShortRatio` | STRING | Ratio of long to short account numbers across all traders |
| `longAccount` | STRING | Proportion of accounts with long positions |
| `shortAccount` | STRING | Proportion of accounts with short positions |
| `timestamp` | LONG | Timestamp (ms) |

---

## 27. Taker Buy/Sell Volume

- **Method:** `GET`
- **Path:** `/futures/data/takerlongshortRatio`
- **Description:** Taker buy/sell volume ratio.
- **Weight:** 0 (rate limit: 1000 requests/5min per IP)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol |
| `period` | ENUM | YES | `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `12h`, `1d` |
| `limit` | LONG | NO | Default 30; max 500 |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |

> Only the data of the latest 30 days is available. If startTime and endTime are not sent, the most recent data is returned.

### Response

```json
[
  {
    "buySellRatio": "0.7120",
    "buyVol": "48923.56",
    "sellVol": "68712.34",
    "timestamp": 1589437530011
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `buySellRatio` | STRING | Ratio of buy volume to sell volume |
| `buyVol` | STRING | Taker buy volume |
| `sellVol` | STRING | Taker sell volume |
| `timestamp` | LONG | Timestamp (ms) |

---

## 28. Basis

- **Method:** `GET`
- **Path:** `/futures/data/basis`
- **Description:** Basis data (difference between futures and index price).
- **Weight:** 0

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `pair` | STRING | YES | Trading pair (e.g. `BTCUSDT`) |
| `contractType` | ENUM | YES | `CURRENT_QUARTER`, `NEXT_QUARTER`, `PERPETUAL` |
| `period` | ENUM | YES | `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `12h`, `1d` |
| `limit` | LONG | NO | Default 30; max 500 |
| `startTime` | LONG | NO | Start timestamp (ms) |
| `endTime` | LONG | NO | End timestamp (ms) |

> Only the data of the latest 30 days is available. If startTime and endTime are not sent, the most recent data is returned.

### Response

```json
[
  {
    "indexPrice": "27103.12",
    "contractType": "PERPETUAL",
    "basisRate": "0.0012",
    "futuresPrice": "27135.50",
    "annualizedBasisRate": "0.0438",
    "basis": "32.38",
    "pair": "BTCUSDT",
    "timestamp": 1589437530011
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `indexPrice` | STRING | Index price |
| `contractType` | STRING | Contract type |
| `basisRate` | STRING | Basis rate |
| `futuresPrice` | STRING | Futures price |
| `annualizedBasisRate` | STRING | Annualized basis rate |
| `basis` | STRING | Basis value (futures price - index price) |
| `pair` | STRING | Trading pair |
| `timestamp` | LONG | Timestamp (ms) |

---

## 29. Composite Index Symbol Information

- **Method:** `GET`
- **Path:** `/fapi/v1/indexInfo`
- **Description:** Get composite index symbol information.
- **Weight:** 1

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | Only for composite index symbols |

### Response

```json
[
  {
    "symbol": "DEFIUSDT",
    "time": 1589437530011,
    "component": "baseAsset",
    "baseAssetList": [
      {
        "baseAsset": "ZRX",
        "quoteAsset": "USDT",
        "weightInQuantity": "78.29",
        "weightInPercentage": "0.0500"
      }
    ]
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Composite index symbol |
| `time` | LONG | Current timestamp (ms) |
| `component` | STRING | Asset component classification |
| `baseAssetList` | ARRAY | Array of constituent assets |

#### baseAssetList Object

| Field | Type | Description |
|-------|------|-------------|
| `baseAsset` | STRING | Underlying asset name |
| `quoteAsset` | STRING | Quote asset (typically `USDT`) |
| `weightInQuantity` | STRING | Quantity-based weighting |
| `weightInPercentage` | STRING | Percentage-based weighting in the index |

---

## 30. Multi-Assets Mode Asset Index

- **Method:** `GET`
- **Path:** `/fapi/v1/assetIndex`
- **Description:** Asset index for Multi-Assets mode.
- **Weight:** 1 (with symbol), 10 (without symbol)

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | Asset pair identifier. When omitted, returns all. |

### Response (single symbol)

```json
{
  "symbol": "BTCUSD",
  "time": 1589437530011,
  "index": "27103.12000000",
  "bidBuffer": "0.0100",
  "askBuffer": "0.0100",
  "bidRate": "26832.09000000",
  "askRate": "27374.15000000",
  "autoExchangeBidBuffer": "0.0200",
  "autoExchangeAskBuffer": "0.0200",
  "autoExchangeBidRate": "26561.06000000",
  "autoExchangeAskRate": "27645.18000000"
}
```

When symbol omitted, response is an array of objects with the same structure.

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Asset pair name |
| `time` | LONG | Timestamp (ms) |
| `index` | STRING | Current index price |
| `bidBuffer` | STRING | Bid buffer percentage |
| `askBuffer` | STRING | Ask buffer percentage |
| `bidRate` | STRING | Bid pricing rate |
| `askRate` | STRING | Ask pricing rate |
| `autoExchangeBidBuffer` | STRING | Auto-exchange bid buffer |
| `autoExchangeAskBuffer` | STRING | Auto-exchange ask buffer |
| `autoExchangeBidRate` | STRING | Auto-exchange bid rate |
| `autoExchangeAskRate` | STRING | Auto-exchange ask rate |

---

## 31. Query Index Price Constituents

- **Method:** `GET`
- **Path:** `/fapi/v1/constituents`
- **Description:** Get index price constituent exchanges and weights.
- **Weight:** 2

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | YES | Trading pair symbol (e.g. `BTCUSDT`) |

### Response

```json
{
  "symbol": "BTCUSDT",
  "time": 1589437530011,
  "constituents": [
    {
      "exchange": "binance",
      "symbol": "BTCUSDT",
      "price": "27103.12",
      "weight": "0.1429"
    },
    {
      "exchange": "coinbase",
      "symbol": "BTC-USD",
      "price": "27105.00",
      "weight": "0.1429"
    }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Index symbol queried |
| `time` | LONG | Timestamp (ms) |
| `constituents` | ARRAY | Array of constituent exchange data |

#### constituents Object

| Field | Type | Description |
|-------|------|-------------|
| `exchange` | STRING | Exchange name (e.g. `binance`, `coinbase`, `gateio`) |
| `symbol` | STRING | Trading pair symbol on that exchange |
| `price` | STRING | Current price (displayed as `-1` for TradFi perps) |
| `weight` | STRING | Weight percentage in index calculation |

---

## 32. Query Insurance Fund Balance Snapshot

- **Method:** `GET`
- **Path:** `/fapi/v1/insuranceBalance`
- **Description:** Get insurance fund balance snapshot.
- **Weight:** 1

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | Filter by specific trading pair |

### Response (with symbol)

```json
{
  "symbols": ["BNBUSDT", "BTCUSDT"],
  "assets": [
    {
      "asset": "USDT",
      "marginBalance": "500000000.00000000",
      "updateTime": 1589437530011
    }
  ]
}
```

### Response (without symbol)

Array of objects, each containing:

```json
[
  {
    "symbols": ["ADAUSDT", "BCHUSDT"],
    "assets": [
      {
        "asset": "USDT",
        "marginBalance": "500000000.00000000",
        "updateTime": 1589437530011
      }
    ]
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `symbols` | ARRAY[STRING] | Affected trading pairs |
| `assets` | ARRAY | Insurance fund assets |

#### assets Object

| Field | Type | Description |
|-------|------|-------------|
| `asset` | STRING | Collateral asset (e.g. `USDT`, `USDC`, `BTC`) |
| `marginBalance` | STRING | Insurance fund balance |
| `updateTime` | LONG | Latest snapshot timestamp (ms) |

---

## 33. Query ADL Risk Rating

- **Method:** `GET`
- **Path:** `/fapi/v1/symbolAdlRisk`
- **Description:** ADL risk rating measures the likelihood of Auto-Deleveraging during liquidation. Updated every 30 minutes.
- **Weight:** 1

### Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `symbol` | STRING | NO | Trading pair. When omitted, returns all symbols. |

### Response (single symbol)

```json
{
  "symbol": "BTCUSDT",
  "adlRisk": "low",
  "updateTime": 1597370495002
}
```

When symbol omitted, response is an array of objects with the same structure.

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | STRING | Trading pair symbol |
| `adlRisk` | STRING | Risk rating: `high`, `medium`, or `low` |
| `updateTime` | LONG | Last update timestamp (ms) |

> The ADL risk rating takes into account: insurance fund balance, position concentration, order book depth, price volatility, average leverage, unrealized PnL, and margin utilization at the symbol level.

---

## 34. Query Trading Schedule

- **Method:** `GET`
- **Path:** `/fapi/v1/tradingSchedule`
- **Description:** Trading session schedules for TradFi Perps underlying assets, covering U.S. equity and commodity markets. Returns a one-week schedule starting from the prior day.
- **Weight:** 5

### Parameters

None.

### Response

```json
{
  "updateTime": 1589437530011,
  "marketSchedules": {
    "EQUITY": {
      "sessions": [
        {
          "startTime": 1589437530011,
          "endTime": 1589523930011,
          "type": "REGULAR"
        }
      ]
    },
    "COMMODITY": {
      "sessions": [
        {
          "startTime": 1589437530011,
          "endTime": 1589523930011,
          "type": "REGULAR"
        }
      ]
    }
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `updateTime` | LONG | Timestamp when schedule was last updated (ms) |
| `marketSchedules` | OBJECT | Contains schedule data by market type |

#### marketSchedules.{EQUITY|COMMODITY}.sessions Object

| Field | Type | Description |
|-------|------|-------------|
| `startTime` | LONG | Session start time (ms) |
| `endTime` | LONG | Session end time (ms) |
| `type` | STRING | Session type: `PRE_MARKET`, `REGULAR`, `AFTER_MARKET`, `OVERNIGHT`, `NO_TRADING` |
