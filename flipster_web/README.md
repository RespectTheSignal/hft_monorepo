# flipster_web (Python)

Flipster 거래소 비공식 주문 실행 모듈. 브라우저 세션 쿠키를 이용한 REST API 클라이언트.

> **Warning**: reverse-engineered 브라우저 API 기반. 공식 API 아님.

## 설치

```bash
git clone https://github.com/RespectTheSignal/hft_monorepo.git
cd hft_monorepo

# Python 의존성
pip install requests websockets
```

### 시스템 의존성 (Ubuntu)

```bash
# 브라우저
sudo apt install xvfb google-chrome-stable

# VNC (sudo 없이 설치)
cd /tmp
apt download x11vnc libvncserver1 libvncclient1
dpkg-deb -x x11vnc_*.deb x11vnc-extract
dpkg-deb -x libvncserver1_*.deb x11vnc-extract
dpkg-deb -x libvncclient1_*.deb x11vnc-extract

# noVNC
git clone --depth 1 https://github.com/novnc/noVNC.git ~/.local/share/noVNC
```

## Quick Start

```python
import sys
sys.path.insert(0, "/path/to/hft_monorepo")
from flipster_web import FlipsterClient, OrderParams, Side, OrderType, MarginType

# 1. 브라우저 실행 (Xvfb + Chrome + VNC)
client = FlipsterClient()
url = client.start_browser()
print(url)  # http://0.0.0.0:6080/vnc.html → 브라우저에서 로그인

# 2. 로그인 완료 후 쿠키 추출
client.login_done()

# 3. 주문
result = client.place_order("BTCUSDT.PERP",
    OrderParams(side=Side.LONG, amount_usd=1.0),
    ref_price=75000.0)

# 4. 청산
slot = result["position"]["slot"]
client.close_position("BTCUSDT.PERP", slot, price=75000.0)

# 5. 종료
client.stop()
```

---

## API Reference

### FlipsterClient

```python
class FlipsterClient:
    def start_browser() -> str
    def login_done()
    def refresh_cookies()
    def place_order(symbol: str, params: OrderParams, ref_price: float = None) -> dict
    def close_position(symbol: str, slot: int, price: float) -> dict
    def raw_request(symbol: str, body: dict) -> dict
    def stop()
```

| Method | Description |
|---|---|
| `start_browser()` | Xvfb + Chrome + x11vnc + noVNC 실행. noVNC URL 반환 |
| `login_done()` | Chrome CDP로 쿠키 추출, HTTP 세션 준비 |
| `refresh_cookies()` | 쿠키 재추출 (`__cf_bm` ~30분 만료 대응) |
| `place_order()` | 주문 실행 (POST) |
| `close_position()` | 포지션 청산 (PUT size=0) |
| `raw_request()` | 디버깅용 raw JSON 전송 |
| `stop()` | 모든 프로세스 종료 |

### OrderParams

```python
@dataclass
class OrderParams:
    side: Side                  # Side.LONG | Side.SHORT
    amount_usd: float           # USD 주문량
    order_type: OrderType       # OrderType.MARKET (default) | OrderType.LIMIT
    price: float | None         # Limit 필수, Market은 ref_price 사용
    leverage: int               # default 1
    margin_type: MarginType     # MarginType.ISOLATED (default) | MarginType.CROSS
```

### Enums

```python
class Side:      LONG = "Long"    | SHORT = "Short"
class OrderType: MARKET           | LIMIT
class MarginType: ISOLATED        | CROSS
```

---

## Flipster REST API 명세 (reverse engineered)

### Open Position

```
POST https://api.flipster.io/api/v2/trade/positions/{symbol}
```

**Request Body:**
```json
{
  "side": "Long" | "Short",
  "requestId": "<uuid-v4>",
  "timestamp": "<unix-nanoseconds>",
  "refServerTimestamp": "<unix-nanoseconds>",
  "refClientTimestamp": "<unix-nanoseconds>",
  "leverage": 1,
  "price": "75000.0",
  "amount": "1",
  "attribution": "SEARCH_PERPETUAL",
  "marginType": "Isolated" | "Cross",
  "orderType": "ORDER_TYPE_MARKET" | "ORDER_TYPE_LIMIT"
}
```

**Response (200):**
```json
{
  "position": {
    "symbol": "BTCUSDT.PERP",
    "slot": 0,
    "avgPrice": "74937.1",
    "liqPrice": "18.9",
    "size": "0.000013",
    "remainingQty": "0",
    "leverage": 1,
    "initMargin": "0.9741823",
    "marginAssigned": "0.973938754425",
    "positionId": "6H5+9/HDphg..."
  },
  "order": {
    "orderId": "073d90dc-2b65-...",
    "symbol": "BTCUSDT.PERP",
    "slot": 0,
    "leverage": 1,
    "side": "Long",
    "price": "74937.1",
    "avgPrice": "74937.1",
    "amount": "1",
    "size": "0.000013",
    "remainingQty": "0",
    "remainingAmount": "0.0258177",
    "action": "Open"
  },
  "timestamp": "1776322547015644661"
}
```

### Close Position

```
PUT https://api.flipster.io/api/v2/trade/positions/{symbol}/{slot}/size
```

**Request Body:**
```json
{
  "requestId": "<uuid-v4>",
  "timestamp": "<unix-nanoseconds>",
  "refServerTimestamp": "<unix-nanoseconds>",
  "refClientTimestamp": "<unix-nanoseconds>",
  "size": "0",
  "price": "74928.8",
  "attribution": "SEARCH_PERPETUAL",
  "orderType": "ORDER_TYPE_MARKET"
}
```

### Required Headers

```
User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0
Accept: application/json, text/plain, */*
Accept-Language: en-US
Referer: https://flipster.io/
Content-Type: application/json
X-Prex-Client-Platform: web
X-Prex-Client-Version: release-web-3.15.110
Origin: https://flipster.io
Sec-GPC: 1
Sec-Fetch-Dest: empty
Sec-Fetch-Mode: cors
Sec-Fetch-Site: same-site
```

### Required Cookies

| Cookie | Description | Expiry |
|---|---|---|
| `session_id_bolts` | 세션 토큰 | 로그인 세션 |
| `session_id_nuts` | 세션 토큰 | 로그인 세션 |
| `ajs_user_id` | 유저 ID | 로그인 세션 |
| `__cf_bm` | Cloudflare 봇 감지 | **~30분** |
| `_ga` | Google Analytics | 장기 |
| `_ga_RH8FM2JKCM` | GA 세션 | 장기 |
| `ajs_anonymous_id` | 익명 추적 ID | 장기 |
| `analytics_session_id` | 세션 분석 | 세션 |
| `internal` | 내부 플래그 | 세션 |
| `referral_path` | 유입 경로 | 세션 |
| `referrer_symbol` | 참조 심볼 | 세션 |

### Error Responses

| Status | Type | Description |
|---|---|---|
| 400 | `TooManyPositions` | 슬롯 초과 (심볼당 최대 ~10) |
| 401 | - | 세션 만료 |
| 403 | - | 접근 거부 |

---

## 성능

| | avg | min |
|---|---|---|
| Open | 120 ms | 87 ms |
| Close | 92 ms | 84 ms |
| Round trip | 213 ms | 171 ms |

---

## 주의사항

- `__cf_bm` 쿠키 ~30분 만료 → `refresh_cookies()` 필요
- `timestamp` 나노초 단위 (19자리)
- `refServerTimestamp` — 현재 클라이언트 타임스탬프로 대체. 서버 검증 시 별도 API 필요할 수 있음
- 심볼당 슬롯 최대 ~10개. 초과 시 `TooManyPositions` 에러
- 연속 주문 시 Cloudflare throttle로 200-300ms 스파이크 발생 가능

## 파일 구조

```
flipster_web/
├── __init__.py      # re-exports
├── browser.py       # BrowserManager (Xvfb + Chrome + x11vnc + noVNC)
├── cookies.py       # CDP WebSocket 쿠키 추출
├── client.py        # FlipsterClient
├── order.py         # OrderParams, Side, MarginType, OrderType
└── README.md
```
