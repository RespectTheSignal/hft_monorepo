# Binance USDⓈ-M Futures API - General Info

## Base URLs

| Environment | Type | URL |
|-------------|------|-----|
| Production | REST | `https://fapi.binance.com` |
| Testnet | REST | `https://demo-fapi.binance.com` |
| Production | WebSocket Streams | `wss://fstream.binance.com` |
| Testnet | WebSocket Streams | `wss://fstream.binancefuture.com` |
| Production | WebSocket API | `wss://ws-fapi.binance.com/ws-fapi/v1` |
| Testnet | WebSocket API | `wss://testnet.binancefuture.com/ws-fapi/v1` |

## API Key & Authentication

- API 키는 `X-MBX-APIKEY` 헤더를 통해 전달
- 키는 대소문자 구분
- API 키 관리: https://www.binance.com/en/support/articles/360002502072

## 요청 형식

### GET 엔드포인트
- 파라미터를 query string으로 전송

### POST/PUT/DELETE 엔드포인트
- query string으로 전송 가능
- request body에 `application/x-www-form-urlencoded` content type으로 전송 가능
- query string과 body 혼합 가능 (동일 파라미터 존재 시 query string 우선)

### 일반 규칙
- 파라미터 순서 무관
- 모든 데이터는 오름차순(oldest first) 반환
- 모든 시간/타임스탬프 필드는 밀리초 단위
- 모든 응답은 JSON 객체 또는 배열

## Endpoint Security Types

| Type | 설명 |
|------|------|
| `NONE` | 인증 불필요 |
| `TRADE` | API Key + signature 필요 |
| `USER_DATA` | API Key + signature 필요 |
| `USER_STREAM` | API Key만 필요 |
| `MARKET_DATA` | API Key 필요 |

## SIGNED Endpoints (TRADE & USER_DATA)

### 서명 요구사항
- `signature` 파라미터를 query string 또는 request body에 포함 필수
- HMAC SHA256 알고리즘 사용
- `totalParams` = query string + request body 연결
- signature는 반드시 마지막 파라미터로 배치
- signature는 대소문자 무관

### Timestamp 요구사항
- 모든 SIGNED 엔드포인트에 `timestamp` 파라미터 필수 (밀리초)
- `recvWindow` 파라미터 선택 (기본값: 5000ms)

### Timing 검증 로직
```
if (timestamp < serverTime + 1000 && serverTime - timestamp <= recvWindow) {
  // 요청 처리
} else {
  // 요청 거부
}
```

### HMAC SHA256 서명 예제

**키 정보:**
```
apiKey: dbefbc809e3e83c283a984c3a1459732ea7db1360ca80c5c2c8867408d28cc83
secretKey: 2b5eb11e18796d12d88f13dc27dbbd02c2cc51ff7059765ed9821957d82bb4d9
```

**파라미터 예시:**
```
symbol=BTCUSDT
side=BUY
type=LIMIT
timeInForce=GTC
quantity=1
price=9000
recvWindow=5000
timestamp=1591702613943
```

**서명 생성:**
```bash
echo -n "symbol=BTCUSDT&side=BUY&type=LIMIT&quantity=1&price=9000&timeInForce=GTC&recvWindow=5000&timestamp=1591702613943" \
  | openssl dgst -sha256 -hmac "2b5eb11e18796d12d88f13dc27dbbd02c2cc51ff7059765ed9821957d82bb4d9"
```

**결과:** `3c661234138461fcc7a7d8746c6558c9842d4e10870d2ecbedf7777cad694af9`

### RSA Key 서명
- PKCS#8 형식 지원
- RSASSA-PKCS1-v1_5 with SHA-256 사용
- 서명은 base64 인코딩 필수
- URL 인코딩 필수 (슬래시, 등호 기호)

## Rate Limits

### 응답 헤더
- `X-MBX-USED-WEIGHT-(intervalNum)(intervalLetter)`: 현재 IP weight
- `X-MBX-ORDER-COUNT-(intervalNum)(intervalLetter)`: 현재 계정 주문 카운트

### Rate Limit 종류
| Type | 설명 |
|------|------|
| `RAW_REQUEST` | 원시 요청 수 |
| `REQUEST_WEIGHT` | 요청 가중치 |
| `ORDER` | 주문 횟수 |

### IP Limit 규칙
- IP 주소 기반 (API 키와 무관)
- 반복 위반 시 자동 차단
- 차단 시간: 2분 ~ 3일 (반복 위반 시 증가)
- 429 응답 수신 시 클라이언트 백오프 의무

### Order Rate Limits
- 계정 단위로 집계
- 거부/실패 주문은 주문 카운트 헤더가 누락될 수 있음

### 권장 사항
> WebSocket 스트림을 사용하여 HTTP 요청 부하를 줄이고 데이터 적시성을 보장하는 것을 권장

### Rate Limit 세부 정보 조회
```
GET /fapi/v1/exchangeInfo
```
응답의 `rateLimits` 배열에서 확인 가능

## HTTP Return Codes

| Code | 설명 |
|------|------|
| 4XX | 잘못된 요청 (클라이언트 오류) |
| 403 | WAF 제한 위반 |
| 408 | 백엔드 응답 타임아웃 |
| 429 | Rate limit 초과 |
| 418 | 429 이후 계속 요청 시 IP 자동 차단 |
| 5XX | 내부 서버 오류 |
| 503 | 서비스 불가 또는 알 수 없는 오류 |

### HTTP 503 처리 방법

#### Type A: "Unknown error, please check your request or try again later."
- 실행 상태 불명 (성공했을 수 있음)
- WebSocket 업데이트 또는 orderId 조회로 확인
- Rate limit 카운트 여부: 불확실

#### Type B: "Service Unavailable."
- 100% 실패
- 지수 백오프로 재시도 (200ms -> 400ms -> 800ms, 최대 3-5회)
- Rate limit 카운트: 미포함

#### Type C: Error -1008 "Request throttled by system-level protection"
- 100% 실패
- 동시성 줄이고 백오프로 재시도
- 적용 대상: `/fapi/v1/order`, `/fapi/v1/batchOrders`, `/fapi/v1/order/test`
- Rate limit 카운트: 미포함
- 예외: Reduce-only/close-position 주문은 면제 및 우선 처리

## Error Response Format
```json
{
  "code": -1121,
  "msg": "Invalid symbol."
}
```

## SDKs

### Python3
- Repository: https://github.com/binance/binance-connector-python
- Install: `pip install binance-sdk-derivatives-trading-usds-futures`

### Java
- Repository: https://github.com/binance/binance-connector-java

> **주의:** 서드파티 제공이며 Binance 공식 제작이 아님. 안전성/성능 보증 없음.

## Postman Collection
- https://github.com/binance-exchange/binance-api-postman
