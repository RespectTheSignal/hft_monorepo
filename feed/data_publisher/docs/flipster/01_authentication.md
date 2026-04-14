# Flipster API - HMAC Authentication Guide

## 개요

모든 인증이 필요한(private) 요청은 HMAC 서명이 필수입니다. 요청 무결성을 보호하고 리플레이 공격을 방지합니다.

## 필수 헤더

| Header | Description |
|--------|-------------|
| `api-key` | 발급받은 API 키 (public key) |
| `api-signature` | 요청에 대한 HMAC 서명 |
| `api-expires` | 요청 만료 Unix timestamp (초 단위) |

## 서명 생성 프로세스

### Step 1: URL 경로 정규화
URL의 path와 query string을 추출합니다.

### Step 2: Canonical Message 구성
```
HTTP_METHOD(대문자) + path(query 포함) + expires_timestamp + request_body(있는 경우)
```

### Step 3: HMAC-SHA256 계산
API secret을 키로 사용하여 HMAC-SHA256을 계산하고, 16진수(hex) 문자열로 인코딩합니다.

## 코드 예제

### Python
```python
import hmac
import hashlib
from urllib.parse import urlparse

def generate_api_key_signature(secret: str, method: str, url: str, 
                               expires: int, data: bytes | None = None) -> str:
    parsed = urlparse(url)
    path = parsed.path + (f"?{parsed.query}" if parsed.query else "")
    
    parts = [
        method.upper().encode("utf-8"),
        path.encode("utf-8"),
        str(expires).encode("utf-8"),
    ]
    if data:
        parts.append(data)
    message = b"".join(parts)
    
    signature = hmac.new(
        secret.encode("utf-8"),
        message,
        digestmod=hashlib.sha256
    ).hexdigest()
    
    return signature
```

### JavaScript
```javascript
function generateApiKeySignature(secret, verb, url, expires, data) {
    const urlObj = new URL(url);
    let path = urlObj.pathname + (urlObj.search ? urlObj.search : "");
    
    let messageStr = verb + path + String(expires);
    if (data !== null && data !== undefined) {
        messageStr = messageStr + data;
    }
    
    const signature = CryptoJS.HmacSHA256(messageStr, secret);
    return CryptoJS.enc.Hex.stringify(signature);
}
```

## 사용 예시 (curl)

```bash
# 1. 만료 시간 설정 (현재 시간 + 60초)
EXPIRES=$(( $(date +%s) + 60 ))

# 2. 서명 생성
SIGNATURE=$(python3 -c "
import hmac, hashlib
secret = 'YOUR_API_SECRET'
method = 'POST'
path = '/api/v1/trade/order'
expires = $EXPIRES
body = '{\"symbol\":\"BTCUSDT.PERP\",\"side\":\"BUY\",\"type\":\"MARKET\",\"quantity\":\"0.001\"}'
message = method + path + str(expires) + body
sig = hmac.new(secret.encode(), message.encode(), hashlib.sha256).hexdigest()
print(sig)
")

# 3. API 호출
curl -X POST https://trading-api.flipster.io/api/v1/trade/order \
  -H "Content-Type: application/json" \
  -H "api-key: YOUR_API_KEY" \
  -H "api-expires: $EXPIRES" \
  -H "api-signature: $SIGNATURE" \
  -d '{"symbol":"BTCUSDT.PERP","side":"BUY","type":"MARKET","quantity":"0.001"}'
```

## 보안 특성

- **변조 방지:** 서명이 요청의 핵심 부분을 커버하므로, 어떤 수정이든 서명을 무효화
- **리플레이 방지:** 타임스탬프(및 선택적 nonce)가 캡처된 요청의 재사용 방지
- **시계 동기화:** `/api/v1/public/time` 엔드포인트를 주기적으로 호출하여 클라이언트-서버 간 시간 차이(drift) 최소화 필요

## 적용 범위

- 모든 private REST 엔드포인트
- WebSocket 인증 (해당하는 경우)
