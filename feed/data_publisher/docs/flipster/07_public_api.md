# Flipster API - Public Endpoints

Public 엔드포인트는 인증 불필요.

---

## 1. Get Server Time

- **Method:** `GET`
- **Path:** `/api/v1/public/time`
- **Description:** 현재 서버 시간을 나노초 단위 Unix 타임스탬프로 반환

### Request Parameters

없음

### Response (200 OK)

```json
{
  "serverTime": "1234567890123456789"
}
```

### Fields

| Field | Type | Description |
|-------|------|-------------|
| `serverTime` | string (Timestamp) | 나노초 수준 고해상도 정수 타임스탬프 |

### 활용

- HMAC 인증 시 `api-expires` 헤더용 시계 동기화
- 클라이언트-서버 간 타임스탬프 드리프트 최소화
- 주기적 호출 권장

### Example

```bash
curl https://trading-api.flipster.io/api/v1/public/time
# Response: {"serverTime":"1704067200000000000"}
```

---

## 2. Check Service Liveliness (Ping)

- **Method:** `GET`
- **Path:** `/api/v1/public/ping`
- **Description:** API 서비스 온라인/응답 상태 확인 (헬스체크)

### Request Parameters

없음

### Response (200 OK)

빈 JSON 객체:
```json
{}
```

### 활용

- 서비스 헬스체크
- 연결 테스트
- 인증 요청 전 서비스 가용성 확인
- 모니터링

### Example

```bash
curl https://trading-api.flipster.io/api/v1/public/ping
# Response: {}
```
