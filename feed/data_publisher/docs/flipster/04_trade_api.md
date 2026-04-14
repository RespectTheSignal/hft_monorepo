# Flipster API - Trade Endpoints

모든 Trade 엔드포인트는 인증(api-key, api-signature, api-expires) 필수.

---

## 1. Place Order

- **Method:** `POST`
- **Path:** `/api/v1/trade/order`
- **Description:** 거래소에 신규 매수/매도 주문을 제출

### Request Body

#### Required Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `symbol` | string | 거래 페어 (예: `BTCUSDT.PERP`). Tradable Symbol List에 존재해야 함 |
| `side` | string | `BUY` 또는 `SELL` |
| `type` | string | `MARKET`, `LIMIT`, `STOP_MARKET` |

#### Optional Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `quantity` | Decimal | - | 기초자산 단위 주문 크기 (예: BTC 수량) |
| `amount` | Decimal | - | 인용자산 단위 주문 크기 (예: USDT 금액) |
| `price` | Decimal | - | LIMIT의 실행 가격 또는 STOP_MARKET의 트리거 가격 |
| `priceMatchStrategy` | string | - | `PRICE_MATCH_STRATEGY_COUNTERPARTY_1`: 최우선호가(BBO)로 가격 설정 |
| `reduceOnly` | boolean | `false` | `true` 시 포지션 축소만 가능, 새 포지션 개설 불가 |
| `maxSlippagePrice` | Decimal | - | 이 가격보다 불리한 체결 방지 |
| `timeInForce` | string | `GTC` | LIMIT 전용: `GTC` (Good Till Cancelled) 또는 `IOC` (Immediate or Cancel). **MARKET 주문에 사용 금지** |

### Request Examples

**Market Buy (1 BTC):**
```json
{
  "symbol": "BTCUSDT.PERP",
  "side": "BUY",
  "type": "MARKET",
  "quantity": "1.0"
}
```

**Limit Sell (0.1 BTC at 90,000):**
```json
{
  "symbol": "BTCUSDT.PERP",
  "side": "SELL",
  "type": "LIMIT",
  "price": "90000",
  "quantity": "0.1"
}
```

**Limit Buy with BBO (Best Bid/Offer):**
```json
{
  "symbol": "BTCUSDT.PERP",
  "side": "BUY",
  "type": "LIMIT",
  "quantity": "0.5",
  "priceMatchStrategy": "PRICE_MATCH_STRATEGY_COUNTERPARTY_1"
}
```

**Stop Market Buy (trigger at 86,000):**
```json
{
  "symbol": "BTCUSDT.PERP",
  "side": "BUY",
  "type": "STOP_MARKET",
  "price": "86000",
  "quantity": "0.1"
}
```

**Limit Buy with IOC:**
```json
{
  "symbol": "BTCUSDT.PERP",
  "side": "BUY",
  "type": "LIMIT",
  "price": "95000",
  "quantity": "0.5",
  "timeInForce": "IOC"
}
```

### Response (200 OK)

```json
{
  "order": {
    "orderId": "uuid",
    "symbol": "BTCUSDT.PERP",
    "side": "BUY",
    "orderType": "MARKET",
    "quantity": "1.0",
    "amount": null,
    "price": null,
    "triggerPrice": null,
    "status": "FILLED",
    "timeInForce": "IOC",
    "takeProfitPrice": null,
    "stopLossPrice": null,
    "leavesQty": "0"
  }
}
```

### Order Response Fields

| Field | Type | Description |
|-------|------|-------------|
| `orderId` | string (UUID) | 고유 주문 식별자 |
| `symbol` | string | 거래 심볼 |
| `side` | string | `BUY` / `SELL` |
| `orderType` | string | `MARKET` / `LIMIT` / `STOP_MARKET` |
| `quantity` | Decimal (nullable) | 기초자산 단위 주문 크기 |
| `amount` | Decimal (nullable) | 인용자산 단위 주문 크기 |
| `price` | Decimal (nullable) | 실행/트리거 가격 |
| `triggerPrice` | Decimal (nullable) | Stop Market의 활성화 가격 |
| `status` | string | `PENDING_NEW`, `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `REJECTED` |
| `timeInForce` | string | `FOK`, `IOC`, `GTC` |
| `takeProfitPrice` | Decimal (nullable) | 익절 가격 |
| `stopLossPrice` | Decimal (nullable) | 손절 가격 |
| `leavesQty` | Decimal (nullable) | 미체결 잔량 |

### Error Responses

| Status | Description |
|--------|-------------|
| 400 | Bad Request: 심볼 미존재, MARKET에 timeInForce 사용, 잘못된 파라미터 |
| 401 | Unauthorized: 잘못되거나 누락된 API 키 |

### 주의사항

1. **Market 주문:** 내부적으로 `IOC`로 처리. `timeInForce` 파라미터 포함 시 400 에러
2. **IOC 동작:** 미체결 부분 즉시 취소. 응답의 `quantity`는 실제 체결량, `status`는 `CANCELLED`
3. **Limit 주문:** `timeInForce` 생략 시 기본값 `GTC`
4. **quantity vs amount:** 둘 중 하나만 지정 (동시 사용 불가)

---

## 2. Set Leverage/Margin Mode

- **Method:** `POST`
- **Path:** `/api/v1/trade/leverage`
- **Description:** 개별 포지션의 레버리지 및 마진 설정 조정

### Request Body (모두 Required)

| Parameter | Type | Description | Constraints |
|-----------|------|-------------|-------------|
| `symbol` | string | 거래 심볼 | Pattern: `^[A-Z0-9]+(\.[A-Z0-9]+)?$` |
| `leverage` | integer | 레버리지 배수 | 1 ~ 100 |
| `marginType` | string | 마진 유형 | `CROSS` 또는 `ISOLATED` |

### Response (200 OK)

```json
{
  "position": {
    "symbol": "string",
    "slot": 0,
    "leverage": 10,
    "marginType": "CROSS",
    "positionSide": "LONG",
    "positionAmount": "string",
    "positionQty": "string",
    "markPrice": "string",
    "unrealizedPnl": "string",
    "initialMargin": "string",
    "maintMargin": "string",
    "entryPrice": "string",
    "breakEvenPrice": "string",
    "liquidationPrice": "string",
    "totalFee": "string",
    "buyableBoosts": [{"duration": 0, "cost": "string"}],
    "boostExpiryTimestamp": "string",
    "boostRemainingCount": 0,
    "boostRealizedPnl": "string"
  }
}
```

### Error Responses

| Status | Description |
|--------|-------------|
| 400 | Bad Request |
| 401 | Unauthorized |

### 주의사항

- Isolated: 마진이 해당 포지션에만 한정, 청산도 해당 포지션만 영향
- Cross: 모든 오픈 포지션 간 리소스 공유
- **주문 전에 반드시 설정 필요**

---

## 3. Adjust Margin

- **Method:** `POST`
- **Path:** `/api/v1/trade/margin`
- **Description:** 기존 포지션의 격리 마진 추가/축소

### Request Body (모두 Required)

| Parameter | Type | Description | Allowed Values |
|-----------|------|-------------|----------------|
| `symbol` | string | 거래 심볼 | Pattern: `^[A-Z0-9]+(\.[A-Z0-9]+)?$` |
| `amount` | string (Decimal) | 마진 조정 금액 | 양수 |
| `type` | integer | 작업 유형 | `1` (마진 추가), `2` (마진 축소) |

### Response (200 OK)

Position 객체 반환 (Set Leverage/Margin Mode와 동일한 스키마)

### Error Responses

| Status | Description |
|--------|-------------|
| 400 | Bad Request (잘못된 파라미터) |
| 401 | Unauthorized |

### 주의사항

- 마진 조정 시 청산 가격 재계산
- 다른 거래를 위한 가용 잔고에 영향
- 포지션 리스크 프로파일 및 레버리지 노출 변경

---

## 4. Set TP/SL (Take Profit / Stop Loss)

- **Method:** `PUT`
- **Path:** `/api/v1/trade/order`
- **Description:** 기존 주문에 TP/SL 가격 설정 또는 수정

### Request Body

#### Required Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `symbol` | string | 거래 심볼. Pattern: `^[A-Z0-9]+(\.[A-Z0-9]+)?$` |
| `orderId` | string (UUID) | 주문 식별자 |

#### Optional Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `newTakeProfitPrice` | Decimal (nullable) | 익절 가격 |
| `newStopLossPrice` | Decimal (nullable) | 손절 가격 |

TP/SL 가격 생략 또는 null 시 해당 값 변경 없음.

### Response (200 OK)

Order 객체 반환 (Place Order와 동일한 스키마)

### Error Responses

| Status | Description |
|--------|-------------|
| 400 | Bad Request |
| 401 | Unauthorized |

### 주의사항 (Critical Warning)

**잘못된 TP/SL 설정은 포지션을 즉시 청산할 수 있음:**
- **Long 포지션:** TP가 현재가 이하 또는 SL이 현재가 이상이면 즉시 트리거
- **Short 포지션:** TP가 현재가 이상 또는 SL이 현재가 이하이면 즉시 트리거

---

## 5. Get Pending Orders

- **Method:** `GET`
- **Path:** `/api/v1/trade/order`
- **Description:** 제출되었으나 아직 실행/취소되지 않은 모든 대기 주문 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | Optional | 거래 심볼 필터 |

### Response (200 OK)

```json
{
  "orders": [
    {
      "orderId": "UUID",
      "symbol": "string",
      "side": "BUY|SELL",
      "orderType": "MARKET|LIMIT|STOP_MARKET",
      "quantity": "Decimal",
      "amount": "Decimal",
      "price": "Decimal",
      "triggerPrice": "Decimal",
      "status": "PENDING_NEW|NEW|PARTIALLY_FILLED|FILLED|CANCELED|REJECTED",
      "timeInForce": "FOK|IOC|GTC",
      "takeProfitPrice": "Decimal",
      "stopLossPrice": "Decimal",
      "leavesQty": "Decimal"
    }
  ]
}
```

### 주의사항

- 대기 주문만 반환. 체결된 오픈 포지션은 `Get Position Info` 참조
- 체결된 포지션 청산: Place Order에서 `reduceOnly: true` 사용

---

## 6. Cancel Pending Order

- **Method:** `DELETE`
- **Path:** `/api/v1/trade/order`
- **Description:** 오더북에 남아있는 미체결 대기 주문 취소

### Request Body (모두 Required)

| Parameter | Type | Description |
|-----------|------|-------------|
| `symbol` | string | 거래 심볼. Pattern: `^[A-Z0-9]+(\.[A-Z0-9]+)?$` |
| `orderId` | string (UUID) | 취소할 주문 식별자 |

### Response (200 OK)

빈 JSON 객체: `{}`

### Error Responses

| Status | Description |
|--------|-------------|
| 400 | Bad Request (잘못된 파라미터) |
| 401 | Unauthorized |

### 주의사항

- 오픈 포지션 청산이 아닌, 대기 주문만 취소
- 부분 체결된 주문의 경우 미체결 부분만 취소
- 포지션 청산: Place Order에서 `reduceOnly: true` 사용

---

## 7. Get Tradable Symbols

- **Method:** `GET`
- **Path:** `/api/v1/trade/symbol`
- **Description:** 플랫폼에서 지원하는 모든 활성 거래 심볼 목록 조회

### Request Parameters

없음

### Response (200 OK)

```json
{
  "spot": ["BTCUSDT", "ETHUSDT"],
  "perpetualSwap": ["BTCUSDT.PERP", "ETHUSDT.PERP"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `spot` | string[] | Spot 거래 심볼 배열 |
| `perpetualSwap` | string[] | 무기한 계약 심볼 배열 |

### Error Responses

| Status | Description |
|--------|-------------|
| 401 | Unauthorized |

---

## 8. Get Order History

- **Method:** `GET`
- **Path:** `/api/v1/trade/order/history`
- **Description:** 커서 기반 페이지네이션으로 과거 주문 이력 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `productType` | string | Optional | `SPOT` 또는 `PERPETUAL` |
| `symbol` | string | Optional | 거래 심볼 필터 |
| `count` | integer | Optional | 반환 항목 수 (기본값: 64, 최소: 1) |
| `before` | string (UUID) | Optional | 이전(오래된) 주문 조회용 커서 |
| `after` | string (UUID) | Optional | 이후(최신) 주문 조회용 커서 |
| `startTime` | string | Optional | 시작 시간 (나노초 타임스탬프, inclusive) |
| `endTime` | string | Optional | 종료 시간 (나노초 타임스탬프, inclusive) |

### Response (200 OK)

```json
{
  "history": [
    {
      "orderId": "UUID",
      "symbol": "string",
      "side": "BUY|SELL",
      "orderType": "MARKET|LIMIT|STOP_MARKET",
      "quantity": "Decimal (nullable)",
      "amount": "Decimal (nullable)",
      "price": "Decimal (nullable)",
      "triggerPrice": "Decimal (nullable)",
      "status": "PENDING_NEW|NEW|PARTIALLY_FILLED|FILLED|CANCELED|REJECTED",
      "orderCancelReason": "string",
      "orderRejectReason": "string",
      "timeInForce": "FOK|IOC|GTC",
      "orderTime": "nanosecond timestamp",
      "transactTime": "nanosecond timestamp",
      "leavesQty": "Decimal (nullable)",
      "cumQty": "Decimal (nullable)",
      "avgPrice": "Decimal (nullable)",
      "cumFee": "Decimal (nullable)",
      "takeProfitPrice": "Decimal (nullable)",
      "stopLossPrice": "Decimal (nullable)",
      "initLeverage": "integer (nullable)",
      "conditionalOrderType": "INVALID|POSITION_TP|POSITION_SL|ORDER_TP|ORDER_SL"
    }
  ],
  "hasPrev": true,
  "hasNext": false
}
```

### 주요 필드 설명

| Field | Description |
|-------|-------------|
| `orderCancelReason` | 취소 사유 |
| `orderRejectReason` | 거부 사유 |
| `orderTime` | 주문 시간 (나노초) |
| `transactTime` | 거래 시간 (나노초) |
| `cumQty` | 누적 체결 수량 |
| `avgPrice` | 평균 체결 가격 |
| `cumFee` | 누적 수수료 |
| `initLeverage` | 초기 레버리지 (Perpetual만 해당) |
| `conditionalOrderType` | 조건부 주문 유형 |

---

## 9. Get Execution History

- **Method:** `GET`
- **Path:** `/api/v1/trade/execution/history`
- **Description:** 커서 기반 페이지네이션으로 과거 체결(trade) 이력 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `productType` | string | Optional | `SPOT` 또는 `PERPETUAL` |
| `symbol` | string | Optional | 거래 심볼 필터 |
| `orderId` | string (UUID) | Optional | 주문 ID 필터 |
| `execType` | string | Optional | 체결 유형 필터: `INVALID`, `NEW`, `TRADE`, `CANCELED`, `REPLACED`, `REJECTED`, `TRIGGERED`, `FUNDING` |
| `count` | integer | Optional | 반환 항목 수 (기본값: 64, 최소: 1) |
| `before` | string (UUID) | Optional | 이전(오래된) 기록 조회용 커서 |
| `after` | string (UUID) | Optional | 이후(최신) 기록 조회용 커서 |
| `startTime` | string | Optional | 시작 시간 (나노초 타임스탬프, inclusive) |
| `endTime` | string | Optional | 종료 시간 (나노초 타임스탬프, inclusive) |

### Response (200 OK)

```json
{
  "history": [
    {
      "execId": "UUID",
      "orderId": "UUID",
      "symbol": "string",
      "side": "BUY|SELL",
      "orderType": "MARKET|LIMIT|STOP_MARKET",
      "price": "Decimal (nullable)",
      "quantity": "Decimal (nullable)",
      "timeInForce": "FOK|IOC|GTC",
      "status": "PENDING_NEW|NEW|PARTIALLY_FILLED|FILLED|CANCELED|REJECTED",
      "execType": "INVALID|NEW|TRADE|CANCELED|REPLACED|REJECTED|TRIGGERED|FUNDING",
      "settlementCurrency": "string",
      "orderTime": "nanosecond timestamp",
      "transactTime": "nanosecond timestamp",
      "lastPrice": "Decimal",
      "lastQty": "Decimal",
      "execFee": "Decimal",
      "cumQty": "Decimal (nullable)",
      "avgPrice": "Decimal (nullable)",
      "conditionalOrderType": "INVALID|POSITION_TP|POSITION_SL|ORDER_TP|ORDER_SL"
    }
  ],
  "hasPrev": false,
  "hasNext": true
}
```

### 주요 필드 설명

| Field | Description |
|-------|-------------|
| `execId` | 체결 고유 ID |
| `execType` | 체결 분류 |
| `settlementCurrency` | 정산 통화 |
| `lastPrice` | 최종 체결 가격 |
| `lastQty` | 최종 체결 수량 |
| `execFee` | 체결 수수료 |
