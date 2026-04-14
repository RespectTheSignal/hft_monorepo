# Flipster API - Account Endpoints

모든 Account 엔드포인트는 인증(api-key) 필수 (read 또는 trade scope).

---

## 1. Get Account Info

- **Method:** `GET`
- **Path:** `/api/v1/account`
- **Description:** 사용자 계정의 종합 정보 조회 (잔고, 미실현 PnL, 마진, 가용 잔고)

### Request Parameters

없음

### Response (200 OK)

```json
{
  "totalWalletBalance": "10000.00",
  "totalUnrealizedPnl": "250.50",
  "totalMarginBalance": "10250.50",
  "totalMarginReserved": "2000.00",
  "availableBalance": "8250.50"
}
```

### Fields

| Field | Type | Description |
|-------|------|-------------|
| `totalWalletBalance` | Decimal | 지갑 총 자산 가치 (미실현 PnL 제외) |
| `totalUnrealizedPnl` | Decimal | 모든 오픈 포지션의 순 미실현 손익 |
| `totalMarginBalance` | Decimal | 총 계정 자본 (잔고 + 미실현 PnL) |
| `totalMarginReserved` | Decimal | 오픈 포지션/주문에 잠겨있는 마진 |
| `availableBalance` | Decimal | 새 포지션 개설 또는 출금 가능 잔고 |

---

## 2. Get Margin Info

- **Method:** `GET`
- **Path:** `/api/v1/account/margin`
- **Description:** 계정의 상세 마진 정보 조회 (Cross/Isolated 마진 분류)

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `currency` | string | Optional | 통화 코드 필터 |

### Response (200 OK)

```json
{
  "margins": [
    {
      "currency": "USDT",
      "isolatedMarginAvailable": "3000.00",
      "unrealizedPnl": "150.25",
      "reservedForIsolatedMargin": "500.00",
      "reservedForIsolatedMarginPosition": "1000.00",
      "crossMarginAvailable": "5000.00",
      "marginBalance": "10000.00",
      "crossMarginRatio": "0.05",
      "initMarginReserved": "1500.00",
      "initPositionMargin": "1000.00"
    }
  ]
}
```

### Margin Fields

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `currency` | string | No | 통화 코드 |
| `isolatedMarginAvailable` | Decimal | No | Isolated Margin 거래 가용 자금 |
| `unrealizedPnl` | Decimal | No | 해당 통화 오픈 포지션의 미실현 손익 |
| `reservedForIsolatedMargin` | Decimal | No | Isolated Margin 주문에 잠긴 자금 |
| `reservedForIsolatedMarginPosition` | Decimal | No | Isolated Margin 포지션 담보 잠금 자금 |
| `crossMarginAvailable` | Decimal | No | Cross Margin 거래 가용 자금 |
| `marginBalance` | Decimal | No | 해당 통화 총 자본 (잔고 + 미실현 PnL) |
| `crossMarginRatio` | Decimal | Yes | Cross Margin 포지션 리스크 비율 (청산 위험 모니터링) |
| `initMarginReserved` | Decimal | No | 모든 오픈 포지션/주문의 총 초기 마진 |
| `initPositionMargin` | Decimal | No | 오픈 포지션에 할당된 마진 부분 |

---

## 3. Get Account Balance

- **Method:** `GET`
- **Path:** `/api/v1/account/balance`
- **Description:** 사용자 계정의 모든 자산 상세 잔고 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `asset` | string | Optional | 통화 코드 필터 |

### Response (200 OK)

Balance 객체 배열:

```json
[
  {
    "asset": "USDT",
    "balance": "10000.00",
    "availableBalance": "8000.00",
    "maxWithdrawableAmount": "7500.00"
  },
  {
    "asset": "BTC",
    "balance": "2.5",
    "availableBalance": "2.0",
    "maxWithdrawableAmount": "1.8"
  }
]
```

### Balance Fields

| Field | Type | Description |
|-------|------|-------------|
| `asset` | string | 통화 코드 |
| `balance` | Decimal | 해당 자산 총 잔고 |
| `availableBalance` | Decimal | 거래 또는 기타 작업에 사용 가능한 잔고 |
| `maxWithdrawableAmount` | Decimal | 출금 가능 최대 금액 |

---

## 4. Get Position Info

- **Method:** `GET`
- **Path:** `/api/v1/account/position`
- **Description:** 사용자의 현재 오픈 포지션 목록 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | Optional | 거래 심볼 필터 |

### Response (200 OK)

Position 객체 배열:

```json
[
  {
    "symbol": "BTCUSDT.PERP",
    "slot": 0,
    "leverage": 10,
    "marginType": "ISOLATED",
    "positionSide": "LONG",
    "positionAmount": "0.5",
    "positionQty": "0.5",
    "markPrice": "42150.80",
    "unrealizedPnl": "250.50",
    "initialMargin": "2107.54",
    "maintMargin": "105.38",
    "entryPrice": "41650.00",
    "breakEvenPrice": "41675.00",
    "liquidationPrice": "39000.00",
    "totalFee": "25.00",
    "buyableBoosts": [],
    "boostExpiryTimestamp": null,
    "boostRemainingCount": 0,
    "boostRealizedPnl": null
  }
]
```

### Position Fields

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `symbol` | string | No | 거래 심볼 |
| `slot` | integer | No | 포지션 슬롯 식별자 |
| `leverage` | integer | No | 적용된 레버리지 |
| `marginType` | string | No | `CROSS` 또는 `ISOLATED` |
| `positionSide` | string | Yes | `LONG` 또는 `SHORT` |
| `positionAmount` | Decimal | Yes | 기초자산 기준 포지션 크기 |
| `positionQty` | Decimal | Yes | 포지션 수량 |
| `markPrice` | Decimal | Yes | PnL 계산용 현재 마크 가격 |
| `unrealizedPnl` | Decimal | Yes | 미실현 손익 |
| `initialMargin` | Decimal | Yes | 포지션 유지를 위해 예약된 마진 |
| `maintMargin` | Decimal | Yes | 청산 방지를 위한 최소 마진 |
| `entryPrice` | Decimal | Yes | 평균 진입 가격 |
| `breakEvenPrice` | Decimal | Yes | 수수료 포함 손익분기 가격 |
| `liquidationPrice` | Decimal | Yes | 자동 청산 가격 |
| `totalFee` | Decimal | Yes | 누적 거래 수수료 |
| `buyableBoosts` | array | No | 사용 가능한 부스트 옵션 |
| `boostExpiryTimestamp` | string | Yes | 활성 부스트 만료 시간 |
| `boostRemainingCount` | integer | No | 남은 부스트 횟수 |
| `boostRealizedPnl` | Decimal | Yes | 부스트 적용 실현 PnL |

---

## 5. Set Trade Mode

- **Method:** `PUT`
- **Path:** `/api/v1/account/trade-mode`
- **Description:** 계정의 거래 모드 설정 (One-Way / Multi-Position)

### Request Body (Required)

| Parameter | Type | Description |
|-----------|------|-------------|
| `tradeMode` | string (enum) | `ONE_WAY` 또는 `MULTIPLE_POSITIONS` |

### Response (200 OK)

```json
{
  "tradeMode": "ONE_WAY"
}
```

### 주의사항

> **중요:** Flipster API는 현재 Perpetual 거래에서 **One-Way 모드만 지원**합니다. 
> `MULTIPLE_POSITIONS` 모드 설정 시 **모든 주문이 처리 불가**합니다.
> 포지션 모드 변경은 주문 및 포지션 관리에 영향을 주므로 **거래 전에 설정** 필요합니다.
