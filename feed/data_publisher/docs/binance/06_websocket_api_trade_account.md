# Binance USDⓈ-M Futures - WebSocket API (Trade & Account)

WebSocket API를 통한 주문 및 계정 관련 요청. REST API와 동일한 기능을 WebSocket을 통해 수행.

Base Endpoint: `wss://ws-fapi.binance.com/ws-fapi/v1`

> **참고:** Ed25519 키만 WebSocket API 서명에 지원됨.

---

## Trade Methods

### 1. New Order - `order.place` (TRADE)

**Weight:** 0

새 주문 전송.

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| side | ENUM | YES | `BUY` / `SELL` |
| positionSide | ENUM | NO | Default `BOTH`; Hedge mode: `LONG`/`SHORT` |
| type | ENUM | YES | `LIMIT`, `MARKET`, `STOP`, `STOP_MARKET`, `TAKE_PROFIT`, `TAKE_PROFIT_MARKET`, `TRAILING_STOP_MARKET` |
| timeInForce | ENUM | NO | `GTC`, `IOC`, `FOK`, `GTD` |
| quantity | DECIMAL | NO | 주문 수량 (`closePosition=true`와 함께 사용 불가) |
| reduceOnly | STRING | NO | `"true"`/`"false"`, default `"false"` |
| price | DECIMAL | NO | 주문 가격 |
| newClientOrderId | STRING | NO | 고유 주문 ID; 자동 생성 가능 |
| stopPrice | DECIMAL | NO | STOP/TAKE_PROFIT 주문용 |
| closePosition | STRING | NO | `"true"`/`"false"`; Close-All 기능 |
| activationPrice | DECIMAL | NO | TRAILING_STOP_MARKET용 |
| callbackRate | DECIMAL | NO | TRAILING_STOP_MARKET용 (0.1-10%) |
| workingType | ENUM | NO | `MARK_PRICE` / `CONTRACT_PRICE` |
| priceProtect | STRING | NO | `"TRUE"`/`"FALSE"`, default `"FALSE"` |
| newOrderRespType | ENUM | NO | `"ACK"` / `"RESULT"`, default `"ACK"` |
| priceMatch | ENUM | NO | `OPPONENT`/`QUEUE` variants; `price`와 함께 사용 불가 |
| selfTradePreventionMode | ENUM | NO | `NONE`, `EXPIRE_TAKER`, `EXPIRE_MAKER`, `EXPIRE_BOTH` |
| goodTillDate | LONG | NO | GTD용 자동 취소 타임스탬프 |
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

#### Request Example
```json
{
  "id": "3f7df6e3-2df4-44b9-9919-d2f38f90a99a",
  "method": "order.place",
  "params": {
    "apiKey": "...",
    "symbol": "BTCUSDT",
    "side": "BUY",
    "type": "LIMIT",
    "timeInForce": "GTC",
    "quantity": 0.1,
    "price": 43187.00,
    "positionSide": "BOTH",
    "timestamp": 1702555533821,
    "signature": "..."
  }
}
```

#### Response Example
```json
{
  "id": "3f7df6e3-2df4-44b9-9919-d2f38f90a99a",
  "status": 200,
  "result": {
    "orderId": 325078477,
    "symbol": "BTCUSDT",
    "status": "NEW",
    "clientOrderId": "iCXL1BywlBaf2sesNUrVl3",
    "price": "43187.00",
    "avgPrice": "0.00",
    "origQty": "0.100",
    "executedQty": "0.000",
    "cumQty": "0.000",
    "cumQuote": "0.00000",
    "timeInForce": "GTC",
    "type": "LIMIT",
    "reduceOnly": false,
    "closePosition": false,
    "side": "BUY",
    "positionSide": "BOTH",
    "stopPrice": "0.00",
    "workingType": "CONTRACT_PRICE",
    "priceProtect": false,
    "origType": "LIMIT",
    "priceMatch": "NONE",
    "selfTradePreventionMode": "NONE",
    "goodTillDate": 0,
    "updateTime": 1702555534435
  },
  "rateLimits": [
    {"rateLimitType": "ORDERS", "interval": "SECOND", "intervalNum": 10, "limit": 300, "count": 1}
  ]
}
```

---

### 2. Modify Order - `order.modify` (TRADE)

**Weight:** 1 (order 10s), 1 (order 1m), 0 (IP)

현재 LIMIT 주문만 수정 가능. 수정된 주문은 매치 큐에서 재정렬됨.

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| orderId | LONG | NO | 주문 ID |
| origClientOrderId | STRING | NO | 원래 client order ID |
| symbol | STRING | YES | Trading pair |
| side | ENUM | YES | `BUY` / `SELL` |
| quantity | DECIMAL | YES | 주문 수량 |
| price | DECIMAL | YES | 주문 가격 |
| priceMatch | ENUM | NO | `OPPONENT`/`QUEUE` variants; `price`와 호환 불가 |
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

**주의사항:**
- `orderId` 또는 `origClientOrderId` 중 하나 필수; `orderId` 우선
- `quantity`와 `price` 모두 전송 필수
- 새 수량 <= executedQty이거나 GTX 주문이 즉시 체결되는 경우 주문 취소
- 주문당 최대 10,000회 수정 가능

---

### 3. Cancel Order - `order.cancel` (TRADE)

**Weight:** 1

활성 주문 취소.

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | 주문 ID |
| origClientOrderId | STRING | NO | 원래 client order ID |
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

`orderId` 또는 `origClientOrderId` 중 하나 필수.

---

### 4. Query Order - `order.status` (USER_DATA)

**Weight:** 1

주문 상태 조회.

**주문 조회 불가 조건:**
- 상태가 `CANCELED`/`EXPIRED`이고 체결 내역 없고 생성 시간 + 3일 < 현재 시간
- 생성 시간 + 90일 < 현재 시간

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | YES | Trading pair |
| orderId | LONG | NO | 주문 ID |
| origClientOrderId | STRING | NO | 원래 client order ID |
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

---

### 5. New Algo Order - `algoOrder.place`

**Weight:** 0

알고 주문 전송 (조건부 주문).

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| algoType | ENUM | YES | `CONDITIONAL`만 지원 |
| symbol | STRING | YES | Trading pair |
| side | ENUM | YES | `BUY` / `SELL` |
| positionSide | ENUM | NO | Default `BOTH`; Hedge mode: `LONG`/`SHORT` |
| type | ENUM | YES | `STOP_MARKET`, `TAKE_PROFIT_MARKET`, `STOP`, `TAKE_PROFIT`, `TRAILING_STOP_MARKET` |
| timeInForce | ENUM | NO | `IOC`, `GTC`, `FOK`, default `GTC` |
| quantity | DECIMAL | NO | `closePosition=true`와 함께 사용 불가 |
| price | DECIMAL | NO | 주문 가격 |
| triggerPrice | DECIMAL | NO | 트리거 가격 |
| workingType | ENUM | NO | `MARK_PRICE` / `CONTRACT_PRICE`, default `CONTRACT_PRICE` |
| priceMatch | ENUM | NO | OPPONENT/QUEUE variants |
| closePosition | STRING | NO | `"true"`/`"false"` |
| priceProtect | STRING | NO | `"TRUE"`/`"FALSE"`, default `"FALSE"` |
| reduceOnly | STRING | NO | `"true"`/`"false"`, default `"false"` |
| activatePrice | DECIMAL | NO | TRAILING_STOP_MARKET용 |
| callbackRate | DECIMAL | NO | TRAILING_STOP_MARKET용 (0.1-10, 1=1%) |
| clientAlgoId | STRING | NO | 고유 ID; 패턴: `^[\.A-Z\:/a-z0-9_-]{1,36}$` |
| newOrderRespType | ENUM | NO | `"ACK"` / `"RESULT"`, default `"ACK"` |
| selfTradePreventionMode | ENUM | NO | STP 모드 |
| goodTillDate | LONG | NO | GTD용 |
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

---

### 6. Cancel Algo Order - `algoOrder.cancel`

**Weight:** 1

활성 알고 주문 취소.

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| algoId | LONG | NO | 알고 주문 ID |
| clientAlgoId | STRING | NO | Client 알고 주문 ID |
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

`algoId` 또는 `clientAlgoId` 중 하나 필수.

---

## Account Methods

### 7. Futures Account Balance V2 - `v2/account.balance` (USER_DATA)

**Weight:** 5

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

#### Response Fields (Array)

| Field | Type | Description |
|-------|------|-------------|
| accountAlias | STRING | 고유 계정 코드 |
| asset | STRING | 자산명 |
| balance | STRING | Wallet balance |
| crossWalletBalance | STRING | Crossed wallet balance |
| crossUnPnl | STRING | Crossed 포지션 미실현 손익 |
| availableBalance | STRING | 사용 가능 잔고 |
| maxWithdrawAmount | STRING | 최대 출금 가능 금액 |
| marginAvailable | BOOLEAN | Multi-Assets 모드에서 마진 사용 가능 여부 |
| updateTime | LONG | 마지막 업데이트 시간 (ms) |

---

### 8. Futures Account Balance - `account.balance` (USER_DATA)

**Weight:** 5

V1 잔고 조회. Response는 V2와 동일.

---

### 9. Account Information V2 - `v2/account.status` (USER_DATA)

**Weight:** 5

현재 계정 정보 조회. Single-asset/Multi-assets 모드에 따라 응답이 다름.

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

#### Response Top-Level Fields

| Field | Type | Description |
|-------|------|-------------|
| totalInitialMargin | STRING | 총 초기 마진 |
| totalMaintMargin | STRING | 총 유지 마진 |
| totalWalletBalance | STRING | 총 지갑 잔고 |
| totalUnrealizedProfit | STRING | 총 미실현 손익 |
| totalMarginBalance | STRING | 총 마진 잔고 |
| totalPositionInitialMargin | STRING | 포지션 초기 마진 |
| totalOpenOrderInitialMargin | STRING | 미체결 주문 초기 마진 |
| totalCrossWalletBalance | STRING | Cross 지갑 잔고 |
| totalCrossUnPnl | STRING | Cross 미실현 손익 |
| availableBalance | STRING | 사용 가능 잔고 |
| maxWithdrawAmount | STRING | 최대 출금 가능 금액 |
| assets | ARRAY | Asset 배열 |
| positions | ARRAY | Position 배열 |

---

### 10. Account Information - `account.status` (USER_DATA)

**Weight:** 5

계정 정보 조회 (V1). 추가 필드 포함:

| Field | Type | Description |
|-------|------|-------------|
| feeTier | INT | Commission tier level |
| canTrade | BOOLEAN | 거래 가능 여부 |
| canDeposit | BOOLEAN | 입금 가능 여부 |
| canWithdraw | BOOLEAN | 출금 가능 여부 |
| multiAssetsMargin | BOOLEAN | Multi-asset 마진 모드 |
| tradeGroupId | INT | Trade group ID |

---

### 11. Position Information V2 - `v2/account.position` (USER_DATA)

**Weight:** 5

현재 포지션 정보 (포지션 또는 미체결 주문이 있는 심볼만 반환).

#### Request Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| symbol | STRING | NO | 생략 시 전체 포지션 |
| recvWindow | LONG | NO | 요청 유효 기간 |
| timestamp | LONG | YES | 타임스탬프 |

#### Response Fields (Array)

| Field | Type | Description |
|-------|------|-------------|
| symbol | STRING | Trading pair |
| positionSide | STRING | `BOTH`, `LONG`, `SHORT` |
| positionAmt | STRING | 포지션 수량 |
| entryPrice | STRING | 평균 진입 가격 |
| breakEvenPrice | STRING | 손익 분기 가격 |
| markPrice | STRING | 현재 Mark price |
| unRealizedProfit | STRING | 미실현 손익 |
| liquidationPrice | STRING | 청산 가격 |
| isolatedMargin | STRING | Isolated 마진 |
| notional | STRING | 포지션 명목 가치 |
| marginAsset | STRING | 마진 자산 |
| isolatedWallet | STRING | Isolated 지갑 잔고 |
| initialMargin | STRING | 초기 마진 |
| maintMargin | STRING | 유지 마진 |
| positionInitialMargin | STRING | 포지션 초기 마진 |
| openOrderInitialMargin | STRING | 미체결 주문 초기 마진 |
| adl | INT | Auto-Deleveraging level |
| bidNotional | STRING | Bid side notional |
| askNotional | STRING | Ask side notional |
| updateTime | LONG | 마지막 업데이트 시간 |

> **참고:** 적시성과 정확성을 위해 User Data Stream `ACCOUNT_UPDATE` 이벤트와 함께 사용 권장.

---

### 12. Position Information - `account.position` (USER_DATA)

**Weight:** 5

구 버전 포지션 정보 조회. 추가 필드:

| Field | Type | Description |
|-------|------|-------------|
| leverage | STRING | 레버리지 배수 |
| maxNotionalValue | STRING | 최대 명목 가치 |
| marginType | STRING | `"isolated"` / `"cross"` |
| isAutoAddMargin | STRING | 자동 마진 추가 상태 |
