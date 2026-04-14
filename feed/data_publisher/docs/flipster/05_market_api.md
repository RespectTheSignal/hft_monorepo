# Flipster API - Market Endpoints

Market 엔드포인트는 실시간 시장 데이터를 제공합니다. 인증(api-key) 필요 (read scope).

---

## 1. Get Contract Info

- **Method:** `GET`
- **Path:** `/api/v1/market/contract`
- **Description:** 플랫폼에 상장된 모든 거래 가능 계약의 상세 정보 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | Optional | 거래 심볼. 생략 시 모든 계약 반환 |

### Response (200 OK)

심볼 지정 시 단일 Contract 객체, 생략 시 Contract 배열 반환.

```json
{
  "symbol": "BTCUSDT.PERP",
  "quoteCurrency": "USDT",
  "initMarginRate": "0.01",
  "maintMarginRate": "0.005",
  "maxLeverage": "100",
  "baseInterestRate": "0.0001",
  "fundingRateCap": "0.0005",
  "fundingIntervalHours": 8,
  "tickSize": "0.1",
  "unitOrderQty": "0.001",
  "notionalMinOrderAmount": "5",
  "notionalMaxOrderAmount": "1000000",
  "notionalMaxPositionAmount": "5000000",
  "maxOpenInterest": "100000"
}
```

### Contract Fields

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `symbol` | string | No | 거래 심볼 |
| `quoteCurrency` | string | No | 호가/정산 통화 |
| `initMarginRate` | Decimal | Yes | 포지션 개설 시 필요한 기본 초기 마진 비율 |
| `maintMarginRate` | Decimal | Yes | 포지션 유지에 필요한 유지 마진 비율 |
| `maxLeverage` | Decimal | Yes | 최대 허용 레버리지 |
| `baseInterestRate` | Decimal | Yes | 펀딩 계산에 사용되는 기준 금리 |
| `fundingRateCap` | Decimal | Yes | 펀딩율 절대 최대값 |
| `fundingIntervalHours` | integer | Yes | 펀딩 교환 빈도 (시간) |
| `tickSize` | Decimal | Yes | 주문의 최소 유효 가격 증가분 |
| `unitOrderQty` | Decimal | Yes | 수량의 최소 단위 |
| `notionalMinOrderAmount` | Decimal | Yes | 주문당 최소 명목 가치 |
| `notionalMaxOrderAmount` | Decimal | Yes | 주문당 최대 명목 가치 |
| `notionalMaxPositionAmount` | Decimal | Yes | 포지션 최대 명목 가치 |
| `maxOpenInterest` | Decimal | Yes | 허용 최대 미결제약정 |

---

## 2. Get Tickers

- **Method:** `GET`
- **Path:** `/api/v1/market/ticker`
- **Description:** 지정 심볼 또는 전체 시장의 실시간 티커 정보 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | Optional | 거래 심볼. 생략 시 모든 마켓 반환 |

### Response (200 OK)

심볼 지정 시 단일 Ticker 객체, 생략 시 Ticker 배열 반환.

```json
{
  "symbol": "BTCUSDT.PERP",
  "volume24h": "15234.5",
  "turnover24h": "450000000.50",
  "priceChange24h": "1250.75",
  "openInterest": "8500.25",
  "bidPrice": "42150.50",
  "askPrice": "42151.00",
  "lastPrice": "42150.75",
  "markPrice": "42150.80",
  "indexPrice": "42149.50",
  "fundingRate": "0.0001",
  "nextFundingTime": "1640000000000000000",
  "fundingIntervalHours": 8,
  "fundingRateCap": "0.0005"
}
```

### Ticker Fields

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `symbol` | string | No | 거래 심볼 |
| `volume24h` | Decimal | Yes | 최근 24시간 총 거래량 |
| `turnover24h` | Decimal | Yes | 최근 24시간 총 거래대금 |
| `priceChange24h` | Decimal | Yes | 최근 24시간 가격 변동(절대값) |
| `openInterest` | Decimal | Yes | 미결제약정 총량 |
| `bidPrice` | Decimal | Yes | 현재 최고 매수호가 |
| `askPrice` | Decimal | Yes | 현재 최저 매도호가 |
| `lastPrice` | Decimal | Yes | 최근 체결가 |
| `markPrice` | Decimal | Yes | 마진 및 청산 계산용 공정 가치 |
| `indexPrice` | Decimal | Yes | 외부 참조 거래소 가중 평균 현물가 |
| `fundingRate` | Decimal | Yes | 현재 정기 펀딩율 |
| `nextFundingTime` | Timestamp | Yes | 다음 펀딩 정산 시각 (나노초) |
| `fundingIntervalHours` | integer | Yes | 연속 펀딩 정산 간 시간 |
| `fundingRateCap` | Decimal | Yes | 허용 최대 펀딩율 |

### 활용

- 시장 요약 대시보드
- 실시간 가격 신호 → 트레이딩 알고리즘
- BookTicker 데이터: `bidPrice`, `askPrice` 필드 활용

---

## 3. Get Funding Info

- **Method:** `GET`
- **Path:** `/api/v1/market/funding-info`
- **Description:** 무기한 계약(Perp) 심볼의 펀딩 정보 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | Optional | 거래 심볼. 생략 시 모든 Perp 심볼 반환 |

### Response (200 OK)

```json
{
  "symbol": "BTCUSDT.PERP",
  "lastFundingRate": "0.00008",
  "fundingRate": "0.0001",
  "nextFundingTime": "1704067200000000000",
  "fundingIntervalHours": 8,
  "fundingRateCap": "0.0005"
}
```

### FundingInfo Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `symbol` | string | Yes | 거래 심볼 |
| `lastFundingRate` | Decimal | Yes | 최근 정산된 펀딩율 |
| `fundingRate` | Decimal | Yes | 다음 정산 예상 펀딩율 |
| `nextFundingTime` | Timestamp | No | 다음 펀딩 정산 시각 (나노초) |
| `fundingIntervalHours` | integer | No | 펀딩 정산 간격 (시간) |
| `fundingRateCap` | Decimal | No | 허용 최대 펀딩율 |

---

## 4. Get Kline (Candlesticks)

- **Method:** `GET`
- **Path:** `/api/v1/market/kline`
- **Description:** 과거 캔들스틱(Kline) 데이터 조회

### Query Parameters (모두 Required, limit 제외)

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | **Yes** | 거래 심볼 |
| `interval` | string | **Yes** | Kline 간격 (아래 참조) |
| `startTime` | string | **Yes** | 시작 시간 (나노초 타임스탬프) |
| `endTime` | string | **Yes** | 종료 시간 (나노초 타임스탬프) |
| `limit` | integer | Optional | 결과 수 제한. 최소: 100, 최대: 1000, 기본값: 100 |

### interval 값

| 값 | 설명 |
|----|------|
| `1` | 1분 |
| `3` | 3분 |
| `5` | 5분 |
| `15` | 15분 |
| `30` | 30분 |
| `60` | 1시간 |
| `120` | 2시간 |
| `180` | 3시간 |
| `240` | 4시간 |
| `360` | 6시간 |
| `720` | 12시간 |
| `1D` | 1일 |
| `1W` | 1주 |
| `1M` | 1월 |

### Response (200 OK)

KlineInterval 배열 (각 요소는 7개 항목의 배열):

```json
[
  [
    "1704067200000000000",  // startTime (나노초 타임스탬프)
    "42000.50",              // open (시가)
    "42500.00",              // high (고가)
    "41800.00",              // low (저가)
    "42300.75",              // close (종가)
    "1500.25",               // volume (거래량)
    "63000000.00"            // turnover (거래대금)
  ]
]
```

| Index | Field | Description |
|-------|-------|-------------|
| 0 | startTime | 시작 시간 (나노초) |
| 1 | open | 시가 |
| 2 | high | 고가 |
| 3 | low | 저가 |
| 4 | close | 종가 |
| 5 | volume | 거래량 |
| 6 | turnover | 거래대금 |

---

## 5. Get Orderbook

- **Method:** `GET`
- **Path:** `/api/v1/market/orderbook`
- **Description:** 지정 거래 페어의 실시간 호가창(오더북) 조회

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `symbol` | string | **Yes** | 거래 심볼. Pattern: `^[A-Z0-9]+(\.[A-Z0-9]+)?$` |
| `limit` | integer | Optional | 반환할 호가 단계 수 (선택적) |

### Response (200 OK)

```json
{
  "symbol": "BTCUSDT.PERP",
  "bids": [
    ["42150.50", "1.5"],
    ["42149.00", "2.0"]
  ],
  "asks": [
    ["42151.00", "1.2"],
    ["42152.00", "3.0"]
  ]
}
```

### Orderbook Fields

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | string | 거래 심볼 |
| `bids` | array | 매수 호가 배열 `[[price, quantity], ...]` |
| `asks` | array | 매도 호가 배열 `[[price, quantity], ...]` |

각 `OrderbookEntry`는 2개 Decimal 문자열의 배열:
- Index 0: 가격
- Index 1: 수량

---

## 6. Get Trading Fee Rate

- **Method:** `GET`
- **Path:** `/api/v1/market/fee-rate`
- **Description:** 사용자 계정의 maker/taker 수수료율 조회

### Request Parameters

없음

### Response (200 OK)

FeeRate 객체 배열:

```json
[
  {
    "level": 1,
    "requiredTradeVolume": "0",
    "requiredAssetBalance": "0",
    "feeRate": "0.0006"
  },
  {
    "level": 2,
    "requiredTradeVolume": "1000000",
    "requiredAssetBalance": "10000",
    "feeRate": "0.0004"
  }
]
```

### FeeRate Fields

| Field | Type | Description |
|-------|------|-------------|
| `level` | integer | 거래 등급 식별자 |
| `requiredTradeVolume` | Decimal | 해당 등급 달성을 위한 최소 30일 거래량 |
| `requiredAssetBalance` | Decimal | 해당 등급 자격을 위한 최소 자산 잔고 |
| `feeRate` | Decimal | 해당 등급의 거래 수수료율 |
