# Flipster API - WebSocket

## 연결 정보

- **Endpoint:** `wss://trading-api.flipster.io/api/v1/stream`
- **Protocol:** WebSocket Secure (WSS)

## 인증

WebSocket 연결 시 핸드셰이크에 REST API와 동일한 인증 헤더 필요:

| Header | Description |
|--------|-------------|
| `api-key` | API 키 ID |
| `api-expires` | Unix timestamp (초 단위) 만료 시간 |
| `api-signature` | HMAC-SHA256 서명 |

### 서명 생성

WebSocket 인증의 서명은 `GET` 메서드와 스트림 엔드포인트 경로를 사용:

```
message = "GET" + "/api/v1/stream" + expires_timestamp
signature = HMAC-SHA256(secret, message)
```

### Python 예제
```python
import hmac
import hashlib
import time
import websockets
import json

api_key = "YOUR_API_KEY"
api_secret = "YOUR_API_SECRET"
expires = int(time.time()) + 60

message = f"GET/api/v1/stream{expires}"
signature = hmac.new(
    api_secret.encode("utf-8"),
    message.encode("utf-8"),
    hashlib.sha256
).hexdigest()

headers = {
    "api-key": api_key,
    "api-expires": str(expires),
    "api-signature": signature
}

async def connect():
    async with websockets.connect(
        "wss://trading-api.flipster.io/api/v1/stream",
        extra_headers=headers
    ) as ws:
        # Subscribe to topics
        await ws.send(json.dumps({
            "op": "subscribe",
            "args": ["ticker.BTCUSDT.PERP"]
        }))
        async for msg in ws:
            data = json.loads(msg)
            print(data)
```

## 구독/해제 형식

### Subscribe
```json
{"op": "subscribe", "args": ["<topic_name>"]}
```

### Unsubscribe
```json
{"op": "unsubscribe", "args": ["<topic>"]}
```

### 복수 토픽 동시 구독
```json
{"op": "subscribe", "args": ["ticker.BTCUSDT.PERP", "orderbook.BTCUSDT.PERP"]}
```

## Available Topics

### Market Data (Public)

| Topic | Format | Description |
|-------|--------|-------------|
| Ticker | `ticker.{symbol}` | 실시간 가격 및 거래량 업데이트 |
| Kline | `kline.{interval}.{symbol}` | 캔들스틱 데이터 (interval: 1, 3, 5, 15, 30, 60, 120, 180, 240, 360, 720, 1D, 1W, 1M) |
| Orderbook | `orderbook.{symbol}` | 오더북 깊이 스냅샷 |

### Account Data (Private)

| Topic | Format | Description |
|-------|--------|-------------|
| Account | `account` | 지갑 잔고 및 미실현 PnL |
| Margin | `account.margin` | 마진 요구사항 및 가용 자금 |
| Balance | `account.balance` | 통화별 자산 잔고 |
| Position | `account.position` | 진입가, 레버리지, PnL 포함 오픈 포지션 |

## 메시지 응답 구조

```json
{
  "topic": "stream_name",
  "ts": "timestamp",
  "data": [
    {
      "actionType": "UPDATE",
      "rows": [/* payload */]
    }
  ]
}
```

- `topic`: 구독한 스트림 이름
- `ts`: 타임스탬프
- `data`: 배열 형태의 데이터
  - `actionType`: `UPDATE` 등 액션 유형
  - `rows`: 실제 페이로드 배열 — **대응하는 REST API 엔드포인트의 스키마와 동일한 구조**

### Ticker 데이터 예시 (ticker.{symbol})
`rows` 필드는 REST API의 `GET /api/v1/market/ticker` 응답과 동일한 스키마:
```json
{
  "topic": "ticker.BTCUSDT.PERP",
  "ts": "1704067200000000000",
  "data": [
    {
      "actionType": "UPDATE",
      "rows": [
        {
          "symbol": "BTCUSDT.PERP",
          "bidPrice": "42150.50",
          "askPrice": "42151.00",
          "lastPrice": "42150.75",
          "markPrice": "42150.80",
          "indexPrice": "42149.50",
          "volume24h": "15234.5",
          "turnover24h": "450000000.50",
          "priceChange24h": "1250.75",
          "openInterest": "8500.25",
          "fundingRate": "0.0001",
          "nextFundingTime": "1640000000000000000",
          "fundingIntervalHours": 8,
          "fundingRateCap": "0.0005"
        }
      ]
    }
  ]
}
```

### Orderbook 데이터 예시 (orderbook.{symbol})
`rows` 필드는 REST API의 `GET /api/v1/market/orderbook` 응답과 동일한 스키마:
```json
{
  "topic": "orderbook.BTCUSDT.PERP",
  "ts": "1704067200000000000",
  "data": [
    {
      "actionType": "UPDATE",
      "rows": [
        {
          "symbol": "BTCUSDT.PERP",
          "bids": [["42150.50", "1.5"], ["42149.00", "2.0"]],
          "asks": [["42151.00", "1.2"], ["42152.00", "3.0"]]
        }
      ]
    }
  ]
}
```

## 참고사항

- Heartbeat/Ping-Pong 메커니즘: 공식 문서에 미기재 (표준 WebSocket ping/pong 프레임 사용 추정)
- 재연결 가이드라인: 공식 문서에 미기재 (연결 끊김 시 재인증 후 재구독 필요)
- Rate Limit: 공식 문서에 미기재
- **실시간 데이터가 필요한 경우 REST polling 대신 WebSocket 스트림 사용 권장** (공식 권장사항)
