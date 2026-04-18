# gate_web

Gate.io 선물 거래소 비공식 주문 실행 모듈. 브라우저 쿠키 기반 인증.

> **Warning**: reverse-engineered 브라우저 API 기반. 공식 API 아님.

## 구조

```
gate_web/
├── python/
│   ├── __init__.py
│   ├── browser.py    # BrowserManager (Xvfb + Chrome + x11vnc + noVNC)
│   ├── cookies.py    # CDP WebSocket 쿠키 추출
│   ├── client.py     # GateClient
│   └── order.py      # OrderParams, Side, OrderType, TimeInForce
└── README.md
```

## 시스템 의존성

flipster_web과 동일:
```bash
sudo apt install xvfb google-chrome-stable
pip install requests websockets
```

## Quick Start

```python
import sys
sys.path.insert(0, "/path/to/hft_monorepo/gate_web")
from python import GateClient, OrderParams, Side, OrderType, TimeInForce

# 1. 브라우저 실행
client = GateClient()
url = client.start_browser()
print(url)  # http://0.0.0.0:6081/vnc.html → 로그인

# 2. 쿠키 추출
client.login_done()

# 3. 로그인 확인
print(client.is_logged_in())  # True

# 4. Market Long 1 contract
result = client.place_order("BTC_USDT",
    OrderParams(side=Side.LONG, size=1))
print(result)  # {"ok": True, "status": 200, "data": {...}, "elapsed_ms": 45.2}

# 5. Limit Short 5 contracts @ $80,000
result = client.place_order("BTC_USDT",
    OrderParams(side=Side.SHORT, size=5,
                order_type=OrderType.LIMIT, price=80000.0))

# 6. 포지션 청산 (close long → sell, so size is negative)
result = client.close_position("BTC_USDT", size=-1)

# 7. 쿠키 갱신
client.refresh_cookies()

# 8. 종료
client.stop()
```

## API

```python
class GateClient:
    start_browser() -> str              # noVNC URL 반환
    login_done()                        # CDP 쿠키 추출
    refresh_cookies()                   # 쿠키 재추출
    check_login() -> dict              # GET /api/web/v1/usercenter/get_info
    is_logged_in() -> bool             # 편의 메서드
    place_order(contract, params) -> dict
    close_position(contract, size, price?) -> dict
    raw_request(body) -> dict          # 디버깅용
    stop()

@dataclass
class OrderParams:
    side: Side                  # Side.LONG | Side.SHORT
    size: int                   # 계약 수 (부호 자동 적용)
    order_type: OrderType       # MARKET (default) | LIMIT
    price: float | None         # Limit 필수
    tif: TimeInForce            # GTC (default) | IOC | POC
    reduce_only: bool           # default False
    text: str | None            # 커스텀 라벨
```

---

## Gate.io Futures REST API 명세

### 인증

브라우저에서 수동 로그인 후 Chrome DevTools Protocol(CDP)로 세션 쿠키 추출.

### Headers

```
User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ...
Accept: application/json
Content-Type: application/json
X-Gate-Applang: en
X-Gate-Device-Type: 0
```

### POST — Place Order

```
POST https://www.gate.com/apiw/v2/futures/usdt/orders
```

**Request Body:**
```json
{
  "contract": "BTC_USDT",
  "size": 1,
  "price": "0",
  "order_type": "market",
  "tif": "gtc",
  "text": "t-1234567890",
  "reduce_only": false
}
```

| Field | Type | Description |
|---|---|---|
| `contract` | string | `"BTC_USDT"`, `"ETH_USDT"` 등 |
| `size` | integer | 양수 = long, 음수 = short |
| `price` | string | `"0"` (market) 또는 지정가 |
| `order_type` | string | `"market"` 또는 `"limit"` |
| `tif` | string | `"gtc"` / `"ioc"` / `"poc"` |
| `text` | string | 커스텀 주문 라벨 |
| `reduce_only` | boolean | 청산 전용 여부 |

**Success Response (200):**
```json
{
  "code": 200,
  "message": "success",
  "data": {
    "id": 123456789,
    "contract": "BTC_USDT",
    "size": 1,
    "price": "75000",
    "fill_price": "74937.1",
    "status": "finished",
    ...
  }
}
```

### Close Position

청산은 같은 endpoint에 `reduce_only: true`로 반대 방향 주문:

```json
{
  "contract": "BTC_USDT",
  "size": -1,
  "price": "0",
  "order_type": "market",
  "tif": "ioc",
  "reduce_only": true
}
```

---

## Errors

| Label | Description |
|---|---|
| `TOO_MANY_REQUEST` | Rate limit — 5분 대기 필요 |
| `ErrorOrderFok` | Fill-or-kill 실패 (성공 취급) |
| `RISK_LIMIT_EXCEEDED` | 리스크 한도 초과 |

---

## flipster_web과의 차이점

| | Gate | Flipster |
|---|---|---|
| **API base** | `www.gate.com/apiw/v2/futures/usdt/orders` | `api.flipster.io/api/v2/trade/positions/{symbol}` |
| **Size 표현** | 부호 있는 정수 (+ long, - short) | USD 금액 + side 필드 |
| **Symbol 형식** | `BTC_USDT` (underscore) | `BTCUSDT.PERP` |
| **인증 갱신** | 쿠키 장기 유효 | `__cf_bm` ~30분 만료 |
| **Rate limit** | `TOO_MANY_REQUEST` 라벨 | Cloudflare 200-300ms 스파이크 |
| **Close 방식** | 같은 endpoint + `reduce_only` | PUT .../positions/{symbol}/{slot}/size |
| **포트** | noVNC 6081, CDP 9223 | noVNC 6080, CDP 9222 |

flipster_web과 동시 실행 가능 (포트 충돌 없음).

---

## 원본 참조

Go 구현체 (order_processor_go): chromedp 기반, gRPC + HTTP 서버.
이 Python 모듈은 동일한 API를 직접 호출하는 경량 구현.
