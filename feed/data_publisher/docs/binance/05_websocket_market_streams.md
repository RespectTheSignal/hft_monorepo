# Binance USDⓈ-M Futures - WebSocket Market Streams

## General WSS Information

### Base Endpoint
`wss://fstream.binance.com`

### Access Patterns
- **단일 스트림:** `wss://fstream.binance.com/ws/<streamName>`
- **복합 스트림:** `wss://fstream.binance.com/stream?streams=<streamName1>/<streamName2>/<streamName3>`

예시:
```
wss://fstream.binance.com/ws/bnbusdt@aggTrade
wss://fstream.binance.com/stream?streams=bnbusdt@aggTrade/btcusdt@markPrice
```

### 스트림 형식
- 모든 심볼은 **소문자**로 제공
- 복합 스트림 이벤트는 래핑됨: `{"stream":"<streamName>","data":<rawPayload>}`

### 연결 제한
- 단일 연결 최대 유효 시간: **24시간** (이후 자동 해제)
- 연결당 최대 **1024개 스트림**
- 수신 메시지: **초당 10개** 제한
- 제한 초과 시 연결 해제; 반복 위반 시 IP 차단 가능

### Ping/Pong
- 서버가 **3분마다** ping 프레임 전송
- 클라이언트는 **10분 이내**에 pong 프레임으로 응답 필수 (미응답 시 연결 해제)
- 비요청 pong 프레임 허용

### Live Subscribe/Unsubscribe

#### SUBSCRIBE
```json
{
  "method": "SUBSCRIBE",
  "params": ["btcusdt@aggTrade", "btcusdt@depth"],
  "id": 1
}
```
Response: `{"result": null, "id": 1}`

#### UNSUBSCRIBE
```json
{
  "method": "UNSUBSCRIBE",
  "params": ["btcusdt@depth"],
  "id": 312
}
```
Response: `{"result": null, "id": 312}`

#### LIST_SUBSCRIPTIONS
```json
{
  "method": "LIST_SUBSCRIPTIONS",
  "id": 3
}
```
Response: `{"result": ["btcusdt@aggTrade"], "id": 3}`

#### SET_PROPERTY
```json
{
  "method": "SET_PROPERTY",
  "params": ["combined", true],
  "id": 5
}
```

#### GET_PROPERTY
```json
{
  "method": "GET_PROPERTY",
  "params": ["combined"],
  "id": 2
}
```

#### Error Codes

| Error Code | Description |
|------------|-------------|
| 0 | Unknown property |
| 1 | Invalid value type: expected Boolean |
| 2 | Multiple validation errors |
| 3 | Invalid JSON |

---

## 1. Aggregate Trade Streams

**Stream Name:** `<symbol>@aggTrade`
**Update Speed:** 100ms

보험 기금 거래 및 ADL 거래는 제외됨.

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"aggTrade"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Symbol |
| `a` | LONG | Aggregate trade ID |
| `p` | STRING | Price |
| `q` | STRING | Quantity (RPI 주문 포함) |
| `nq` | STRING | Quantity (RPI 주문 제외) |
| `f` | LONG | First trade ID |
| `l` | LONG | Last trade ID |
| `T` | LONG | Trade time (ms) |
| `m` | BOOLEAN | Is the buyer the market maker? |

```json
{
  "e": "aggTrade",
  "E": 1672515782136,
  "s": "BTCUSDT",
  "a": 123456,
  "p": "16800.50",
  "q": "0.500",
  "nq": "0.450",
  "f": 100,
  "l": 105,
  "T": 1672515782100,
  "m": true
}
```

---

## 2. Mark Price Stream

**Stream Name:** `<symbol>@markPrice` (3초) 또는 `<symbol>@markPrice@1s` (1초)
**Update Speed:** 3000ms 또는 1000ms

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"markPriceUpdate"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Symbol |
| `p` | STRING | Mark price |
| `ap` | STRING | Mark price moving average |
| `i` | STRING | Index price |
| `P` | STRING | Estimated settle price (정산 1시간 전에만 유효) |
| `r` | STRING | Funding rate |
| `T` | LONG | Next funding time (ms) |

```json
{
  "e": "markPriceUpdate",
  "E": 1562305380000,
  "s": "BTCUSDT",
  "p": "11794.15000000",
  "ap": "11794.15000000",
  "i": "11784.62659091",
  "P": "11784.25641265",
  "r": "0.00038167",
  "T": 1562306400000
}
```

---

## 3. Mark Price Stream for All Market

**Stream Name:** `!markPrice@arr` (3초) 또는 `!markPrice@arr@1s` (1초)
**Update Speed:** 3000ms 또는 1000ms

모든 심볼의 Mark price 및 funding rate. TradFi 심볼은 별도 메시지로 전송.

Payload: Mark Price Stream과 동일한 객체 배열.

---

## 4. Kline/Candlestick Streams

**Stream Name:** `<symbol>@kline_<interval>`
**Update Speed:** 250ms

### 지원 Interval
`1m`, `3m`, `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `8h`, `12h`, `1d`, `3d`, `1w`, `1M`

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"kline"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Symbol |
| `k.t` | LONG | Kline start time |
| `k.T` | LONG | Kline close time |
| `k.s` | STRING | Symbol |
| `k.i` | STRING | Interval |
| `k.f` | LONG | First trade ID |
| `k.L` | LONG | Last trade ID |
| `k.o` | STRING | Open price |
| `k.c` | STRING | Close price |
| `k.h` | STRING | High price |
| `k.l` | STRING | Low price |
| `k.v` | STRING | Base asset volume |
| `k.n` | INT | Number of trades |
| `k.x` | BOOLEAN | Is this kline closed? |
| `k.q` | STRING | Quote asset volume |
| `k.V` | STRING | Taker buy base asset volume |
| `k.Q` | STRING | Taker buy quote asset volume |
| `k.B` | STRING | Ignore |

---

## 5. Continuous Contract Kline/Candlestick Streams

**Stream Name:** `<pair>_<contractType>@continuousKline_<interval>`
**Update Speed:** 250ms

### 지원 Contract Types
- `perpetual`
- `current_quarter`
- `next_quarter`
- `tradifi_perpetual`

### 지원 Interval
`1s`, `1m`, `3m`, `5m`, `15m`, `30m`, `1h`, `2h`, `4h`, `6h`, `8h`, `12h`, `1d`, `3d`, `1w`, `1M`

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"continuous_kline"` |
| `E` | LONG | Event time (ms) |
| `ps` | STRING | Pair |
| `ct` | STRING | Contract type |
| `k.t` | LONG | Kline start time |
| `k.T` | LONG | Kline close time |
| `k.i` | STRING | Interval |
| `k.f` | LONG | First trade ID |
| `k.L` | LONG | Last trade ID |
| `k.o` | STRING | Open price |
| `k.c` | STRING | Close price |
| `k.h` | STRING | High price |
| `k.l` | STRING | Low price |
| `k.v` | STRING | Volume |
| `k.n` | INT | Number of trades |
| `k.x` | BOOLEAN | Is this kline closed? |
| `k.q` | STRING | Quote asset volume |
| `k.V` | STRING | Taker buy volume |
| `k.Q` | STRING | Taker buy quote asset volume |

---

## 6. Individual Symbol Mini Ticker Stream

**Stream Name:** `<symbol>@miniTicker`
**Update Speed:** 2000ms

24시간 롤링 윈도우 미니 티커 통계 (UTC 일간 통계 아님).

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"24hrMiniTicker"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Symbol |
| `c` | STRING | Close price |
| `o` | STRING | Open price |
| `h` | STRING | High price |
| `l` | STRING | Low price |
| `v` | STRING | Total traded base asset volume |
| `q` | STRING | Total traded quote asset volume |

---

## 7. All Market Mini Tickers Stream

**Stream Name:** `!miniTicker@arr`
**Update Speed:** 1000ms

모든 심볼의 미니 티커. 변경된 심볼만 포함.

---

## 8. Individual Symbol Ticker Streams

**Stream Name:** `<symbol>@ticker`
**Update Speed:** 2000ms

24시간 롤링 윈도우 티커 통계 (UTC 일간 통계 아님).

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"24hrTicker"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Symbol |
| `p` | STRING | Price change |
| `P` | STRING | Price change percent |
| `w` | STRING | Weighted average price |
| `c` | STRING | Last price |
| `Q` | STRING | Last quantity |
| `o` | STRING | Open price |
| `h` | STRING | High price |
| `l` | STRING | Low price |
| `v` | STRING | Total traded base asset volume |
| `q` | STRING | Total traded quote asset volume |
| `O` | LONG | Statistics open time (ms) |
| `C` | LONG | Statistics close time (ms) |
| `F` | LONG | First trade ID |
| `L` | LONG | Last trade ID |
| `n` | INT | Total number of trades |

---

## 9. All Market Tickers Streams

**Stream Name:** `!ticker@arr`
**Update Speed:** 1000ms

모든 심볼의 티커. 변경된 심볼만 포함.

---

## 10. Individual Symbol Book Ticker Streams (HFT 핵심)

**Stream Name:** `<symbol>@bookTicker`
**Update Speed:** **Real-time**

최적 매수/매도 호가 또는 수량 변경 시 실시간 푸시. RPI 주문 제외.

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"bookTicker"` |
| `u` | LONG | Order book updateId |
| `E` | LONG | Event time (ms) |
| `T` | LONG | Transaction time (ms) |
| `s` | STRING | Symbol |
| `b` | STRING | Best bid price |
| `B` | STRING | Best bid qty |
| `a` | STRING | Best ask price |
| `A` | STRING | Best ask qty |

```json
{
  "e": "bookTicker",
  "u": 400900217,
  "E": 1568014460893,
  "T": 1568014460891,
  "s": "BNBUSDT",
  "b": "25.35190000",
  "B": "31.21000000",
  "a": "25.36520000",
  "A": "40.66000000"
}
```

---

## 11. All Book Tickers Stream

**Stream Name:** `!bookTicker`
**Update Speed:** 5초

모든 심볼의 최적 매수/매도 호가. RPI 주문 제외.

Payload: Individual Symbol Book Ticker와 동일.

---

## 12. Liquidation Order Streams

**Stream Name:** `<symbol>@forceOrder`
**Update Speed:** 1000ms

강제 청산 주문 스냅샷. 각 1000ms 윈도우 내 가장 큰 청산 주문만 푸시. 청산 없으면 데이터 없음.

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"forceOrder"` |
| `E` | LONG | Event time (ms) |
| `o.s` | STRING | Symbol |
| `o.S` | STRING | Side (`"SELL"` / `"BUY"`) |
| `o.o` | STRING | Order type (`"LIMIT"`) |
| `o.f` | STRING | Time in force (`"IOC"`) |
| `o.q` | STRING | Original quantity |
| `o.p` | STRING | Price |
| `o.ap` | STRING | Average price |
| `o.X` | STRING | Order status (`"FILLED"`) |
| `o.l` | STRING | Order last filled quantity |
| `o.z` | STRING | Order filled accumulated quantity |
| `o.T` | LONG | Order trade time (ms) |

---

## 13. All Market Liquidation Order Streams

**Stream Name:** `!forceOrder@arr`
**Update Speed:** 1000ms

모든 심볼의 강제 청산 주문. Payload는 위와 동일.

---

## 14. Partial Book Depth Streams

**Stream Name:** `<symbol>@depth<levels>` 또는 `<symbol>@depth<levels>@500ms` 또는 `<symbol>@depth<levels>@100ms`
**Update Speed:** 250ms, 500ms, 또는 100ms
**Valid Levels:** 5, 10, 20

상위 매수/매도 호가. RPI 주문 제외.

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"depthUpdate"` |
| `E` | LONG | Event time (ms) |
| `T` | LONG | Transaction time (ms) |
| `s` | STRING | Symbol |
| `U` | LONG | First update ID in event |
| `u` | LONG | Final update ID in event |
| `pu` | LONG | Final update ID of previous event |
| `b` | ARRAY | Bids `[[price, qty], ...]` |
| `a` | ARRAY | Asks `[[price, qty], ...]` |

```json
{
  "e": "depthUpdate",
  "E": 1571889248277,
  "T": 1571889248276,
  "s": "BTCUSDT",
  "U": 390497796,
  "u": 390497878,
  "pu": 390497794,
  "b": [["7403.89", "0.002"], ["7403.90", "3.906"]],
  "a": [["7405.96", "3.340"], ["7406.63", "4.525"]]
}
```

---

## 15. Diff. Book Depth Streams

**Stream Name:** `<symbol>@depth` 또는 `<symbol>@depth@500ms` 또는 `<symbol>@depth@100ms`
**Update Speed:** 250ms, 500ms, 또는 100ms

오더북 가격 및 수량 업데이트. RPI 주문 제외.

Payload: Partial Book Depth Streams와 동일.

### 로컬 오더북 관리 방법

1. `wss://fstream.binance.com/stream?streams=btcusdt@depth` 스트림 연결
2. 수신 이벤트 버퍼링. 동일 가격의 경우 최신 업데이트가 이전 것을 덮어씀
3. REST API로 depth 스냅샷 요청: `GET /fapi/v1/depth?symbol=BTCUSDT&limit=1000`
4. 스냅샷의 `lastUpdateId`보다 작은 `u`를 가진 이벤트는 버림
5. 첫 번째 처리 이벤트: `U` <= `lastUpdateId` AND `u` >= `lastUpdateId`
6. 각 새 이벤트의 `pu`가 이전 이벤트의 `u`와 같아야 함, 아니면 step 3부터 재초기화
7. 각 이벤트의 데이터는 해당 가격 레벨의 **절대** 수량 (델타 아님)
8. 수량이 0이면 해당 가격 레벨 **제거**
9. 로컬 오더북에 없는 가격 레벨 제거 이벤트 수신은 정상

---

## 16. RPI Diff Book Depth Streams

**Stream Name:** `<symbol>@rpiDepth@500ms`
**Update Speed:** 500ms

RPI(Retail Price Improvement) 주문이 포함된 매수/매도 호가. 가격 레벨 수량이 0이면 완전 체결/취소 또는 해당 레벨에 숨겨진 크로스 RPI 주문을 의미.

Payload: Diff. Book Depth와 동일.

---

## 17. Composite Index Symbol Information Streams

**Stream Name:** `<symbol>@compositeIndex`
**Update Speed:** 1000ms

인덱스 심볼의 복합 인덱스 정보.

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"compositeIndex"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Symbol |
| `p` | STRING | Composite index price |
| `C` | STRING | Composition type |
| `c` | ARRAY | Component 배열 |

#### Component Object

| Field | Type | Description |
|-------|------|-------------|
| `b` | STRING | Base asset |
| `q` | STRING | Quote asset |
| `w` | STRING | Weight quantity |
| `W` | STRING | Weight percentage |
| `i` | STRING | Index price |

---

## 18. Contract Info Stream

**Stream Name:** `!contractInfo`
**Update Speed:** Real-time (이벤트 기반)

컨트랙트 정보 변경 시 푸시: 상장, 정산, 브라켓 업데이트. `bks` 필드는 브라켓 수정 시에만 표시.

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"contractInfo"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Symbol |
| `ps` | STRING | Pair |
| `ct` | STRING | Contract type (예: `"PERPETUAL"`) |
| `dt` | LONG | Delivery date time (ms) |
| `ot` | LONG | Onboard date time (ms) |
| `cs` | STRING | Contract status (예: `"TRADING"`) |
| `bks` | ARRAY | Bracket 정보 (브라켓 업데이트 시에만) |

#### Bracket Object

| Field | Type | Description |
|-------|------|-------------|
| `bs` | INT | Notional bracket |
| `bnf` | FLOAT | Floor notional |
| `bnc` | FLOAT | Cap notional |
| `mmr` | FLOAT | Maintenance margin ratio |
| `cf` | FLOAT | Auxiliary number |
| `mi` | INT | Min leverage |
| `ma` | INT | Max leverage |

---

## 19. Multi-Assets Mode Asset Index Stream

**Stream Name:** `<assetSymbol>@assetIndex` (개별) 또는 `!assetIndex@arr` (전체)
**Update Speed:** 1000ms

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"assetIndexUpdate"` |
| `E` | LONG | Event time (ms) |
| `s` | STRING | Asset symbol |
| `i` | STRING | Index price |
| `b` | STRING | Bid buffer |
| `a` | STRING | Ask buffer |
| `B` | STRING | Bid rate |
| `A` | STRING | Ask rate |
| `q` | STRING | Auto exchange bid buffer |
| `g` | STRING | Auto exchange ask buffer |
| `Q` | STRING | Auto exchange bid rate |
| `G` | STRING | Auto exchange ask rate |

---

## 20. Trading Session Stream

**Stream Name:** `tradingSession`
**Update Speed:** 1000ms

TradFi Perpetual 컨트랙트의 기초자산 거래 세션 정보.

### Session Types
- **주식:** `PRE_MARKET`, `REGULAR`, `AFTER_MARKET`, `OVERNIGHT`, `NO_TRADING`
- **원자재:** `REGULAR`, `NO_TRADING`

### Payload

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `"EquityUpdate"` 또는 `"CommodityUpdate"` |
| `E` | LONG | Event time (ms) |
| `t` | LONG | Session start time (ms) |
| `T` | LONG | Session end time (ms) |
| `S` | STRING | Session type |

---

## Quick Reference: 전체 스트림 요약

| # | Stream | Name Format | Update Speed |
|---|--------|------------|-------------|
| 1 | Aggregate Trade | `<symbol>@aggTrade` | 100ms |
| 2 | Mark Price | `<symbol>@markPrice[@1s]` | 3s / 1s |
| 3 | Mark Price (All) | `!markPrice@arr[@1s]` | 3s / 1s |
| 4 | Kline | `<symbol>@kline_<interval>` | 250ms |
| 5 | Continuous Kline | `<pair>_<contractType>@continuousKline_<interval>` | 250ms |
| 6 | Mini Ticker | `<symbol>@miniTicker` | 2s |
| 7 | All Mini Tickers | `!miniTicker@arr` | 1s |
| 8 | Ticker | `<symbol>@ticker` | 2s |
| 9 | All Tickers | `!ticker@arr` | 1s |
| 10 | **Book Ticker** | `<symbol>@bookTicker` | **Real-time** |
| 11 | All Book Tickers | `!bookTicker` | 5s |
| 12 | Liquidation Order | `<symbol>@forceOrder` | 1s |
| 13 | All Liquidation Orders | `!forceOrder@arr` | 1s |
| 14 | Partial Depth | `<symbol>@depth<levels>[@500ms\|@100ms]` | 250ms/500ms/100ms |
| 15 | Diff Depth | `<symbol>@depth[@500ms\|@100ms]` | 250ms/500ms/100ms |
| 16 | RPI Diff Depth | `<symbol>@rpiDepth@500ms` | 500ms |
| 17 | Composite Index | `<symbol>@compositeIndex` | 1s |
| 18 | Contract Info | `!contractInfo` | Real-time |
| 19 | Asset Index | `<asset>@assetIndex` / `!assetIndex@arr` | 1s |
| 20 | Trading Session | `tradingSession` | 1s |
