# Binance USDⓈ-M Futures - WebSocket API General Info

## Base Endpoints

| Environment | URL |
|-------------|-----|
| Production | `wss://ws-fapi.binance.com/ws-fapi/v1` |
| Testnet | `wss://testnet.binancefuture.com/ws-fapi/v1` |

## 연결 규칙

- 단일 연결 최대 유효 시간: **24시간** (이후 자동 해제)
- 서버가 **3분마다** ping 프레임 전송
- 클라이언트는 **10분 이내**에 pong 프레임으로 응답해야 함 (미응답 시 연결 해제)
- 비요청 pong의 payload는 비워야 함
- WebSocket 핸드셰이크 시도: **weight 5**

## 데이터 형식 요구사항

- 요청/응답은 JSON 형식의 텍스트 프레임으로 전송 (프레임당 하나)
- `INT` 파라미터 (타임스탬프): JSON 정수, 문자열 아님
- `DECIMAL` 파라미터 (가격): JSON **문자열**, float 아님
- 모든 필드명과 값은 대소문자 구분
- 타임스탬프는 밀리초 단위 UTC
- 리스트는 시간순 반환 (별도 명시 없는 한)

## 요청 형식

```json
{
  "id": "arbitrary_identifier",
  "method": "method.name",
  "params": { }
}
```

- 요청 ID는 세션 내 재사용 가능 (동시 중복 ID는 피할 것)
- 메서드 이름에 명시적 버전 접두사 사용 가능 (예: `"v3/order.place"`)
- 파라미터 순서 무관

## 응답 형식

```json
{
  "id": "matching_request_id",
  "status": 200,
  "result": { },
  "rateLimits": [ ]
}
```

에러 시:
```json
{
  "id": "matching_request_id",
  "status": 400,
  "error": {
    "code": -1102,
    "msg": "Mandatory parameter 'side' was not sent..."
  },
  "rateLimits": [ ]
}
```

### 응답 필드
| 필드 | 설명 |
|------|------|
| `id` | 요청 ID와 일치 |
| `status` | HTTP 스타일 상태 코드 |
| `result` | 성공 시 응답 내용 |
| `error` | 실패 시 에러 코드 및 메시지 |
| `rateLimits` | Rate limit 상태 배열 (선택적, 제어 가능) |

## 인증 방법

### Session 기반 인증

| 메서드 | 설명 | Weight |
|--------|------|--------|
| `session.logon` | API 키로 연결 인증 | 2 |
| `session.status` | 현재 인증 상태 확인 | 2 |
| `session.logout` | 인증된 API 키 초기화 | 2 |

### Ad Hoc 인증
- 개별 요청에 `apiKey`와 `signature` 파라미터를 지정하여 세션 인증 오버라이드

## 서명 생성

- **Ed25519 키만** WebSocket 인증에 지원
- 서명 페이로드: 모든 요청 파라미터를 이름 기준 알파벳순으로 정렬 (signature 자체 제외) 후 서명
- 형식: `param1=value1&param2=value2` (알파벳 정렬)

## Rate Limiting

- REST API와 rate limit **공유**
- Ping/pong 프레임 제한: 최대 **초당 5개**
- 응답의 `rateLimits` 배열에 rate limit 정보 포함
- `returnRateLimits` 파라미터로 가시성 제어

**연결 문자열에서 설정:**
```
wss://ws-fapi.binance.com/ws-fapi/v1?returnRateLimits=false
```

**개별 요청에서 설정:**
```json
{
  "id": "1",
  "method": "order.place",
  "params": {
    "returnRateLimits": true
  }
}
```

## API Key 폐기

세션 중 API 키가 무효화되면 (IP 화이트리스트 미등록, 키 삭제, 권한 부족 등):
- 다음 요청 시 status 401, error code -2015 반환
- 메시지: `"Invalid API-key, IP, or permissions for action."`

## User Data Streams
- 별도 WebSocket 연결 필요
- 메인 API 연결에서는 User Data Stream 구독 불가
