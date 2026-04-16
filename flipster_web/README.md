# flipster_web

Flipster 거래소 비공식 주문 실행 모듈. Python과 Rust 두 가지 구현 제공.

> **Warning**: reverse-engineered 브라우저 API 기반. 공식 API 아님.

## 구조

```
flipster_web/
├── python/           # Python 구현
│   ├── __init__.py
│   ├── browser.py    # BrowserManager (Xvfb + Chrome + x11vnc + noVNC)
│   ├── cookies.py    # CDP WebSocket 쿠키 추출
│   ├── client.py     # FlipsterClient
│   └── order.py      # OrderParams, Side, MarginType, OrderType
├── rust/             # Rust 구현
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── browser.rs
│       ├── cookies.rs
│       ├── client.rs
│       ├── order.rs
│       └── error.rs
└── README.md         # 이 파일
```

## 어떤 걸 쓸까?

| | Python | Rust |
|---|---|---|
| Open avg | 120 ms | **79 ms** |
| Open min | 87 ms | **64 ms** |
| 편의성 | 높음 (스크립팅) | 보통 (빌드 필요) |
| 용도 | 빠른 테스트, 프로토타이핑 | 프로덕션, 저레이턴시 |

min ~62ms는 서버 매칭엔진 처리 한계. 두 구현 모두 동일한 API를 사용.

---

## 시스템 의존성 (공통)

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

---

## Python 사용법

### 설치

```bash
pip install requests websockets
```

### Quick Start

```python
import sys
sys.path.insert(0, "/path/to/hft_monorepo/flipster_web")
from python import FlipsterClient, OrderParams, Side, OrderType, MarginType

# 1. 브라우저 실행
client = FlipsterClient()
url = client.start_browser()
print(url)  # http://0.0.0.0:6080/vnc.html → 로그인

# 2. 쿠키 추출
client.login_done()

# 3. Market Long $1
result = client.place_order("BTCUSDT.PERP",
    OrderParams(side=Side.LONG, amount_usd=1.0),
    ref_price=75000.0)

# 4. Limit Short $5 @ $80,000
result = client.place_order("BTCUSDT.PERP",
    OrderParams(side=Side.SHORT, amount_usd=5.0,
                order_type=OrderType.LIMIT, price=80000.0))

# 5. 청산
slot = result["position"]["slot"]
client.close_position("BTCUSDT.PERP", slot, price=80000.0)

# 6. 쿠키 갱신 (__cf_bm ~30분 만료)
client.refresh_cookies()

# 7. 종료
client.stop()
```

### Python API

```python
class FlipsterClient:
    start_browser() -> str              # VNC URL 반환
    login_done()                        # CDP 쿠키 추출
    refresh_cookies()                   # 쿠키 재추출
    place_order(symbol, params, ref_price?) -> dict
    close_position(symbol, slot, price) -> dict
    raw_request(symbol, body) -> dict   # 디버깅용
    stop()

@dataclass
class OrderParams:
    side: Side                  # Side.LONG | Side.SHORT
    amount_usd: float           # USD
    order_type: OrderType       # MARKET (default) | LIMIT
    price: float | None         # Limit 필수
    leverage: int               # default 1
    margin_type: MarginType     # ISOLATED (default) | CROSS
```

---

## Rust 사용법

### 설치

```toml
# git 의존성
[dependencies]
flipster-client = { git = "https://github.com/RespectTheSignal/hft_monorepo.git", path = "flipster_web/rust" }
```

### Quick Start

```rust
use flipster_client::{BrowserManager, FlipsterClient, OrderParams, OrderType, Side};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 브라우저 실행
    let mut browser = BrowserManager::new();
    let url = browser.start()?;
    println!("로그인: {url}");

    // 2. 쿠키 추출
    let ws_url = browser.cdp_ws_url().await?;
    let cookies = flipster_client::extract_cookies(&ws_url).await?;
    let client = FlipsterClient::from_cookies(&cookies);

    // 3. Market Long $1
    let r = client.place_order("BTCUSDT.PERP",
        OrderParams::builder()
            .side(Side::Long)
            .price(75000.0)
            .amount(1.0)
            .order_type(OrderType::Market)
            .build()
    ).await?;
    let slot = r.raw["position"]["slot"].as_u64().unwrap() as u32;

    // 4. Limit Short $5 @ $80,000
    let r = client.place_order("BTCUSDT.PERP",
        OrderParams::builder()
            .side(Side::Short)
            .price(80000.0)
            .amount(5.0)
            .order_type(OrderType::Limit)
            .build()
    ).await?;

    // 5. 청산
    client.close_position("BTCUSDT.PERP", slot, 75000.0).await?;

    // 6. 쿠키 갱신
    let cookies = flipster_client::extract_cookies(&ws_url).await?;
    client.reload_cookies(&cookies);

    // 7. 종료
    browser.stop();
    Ok(())
}
```

### Rust API

```rust
// 브라우저
BrowserManager::new() -> Self
BrowserManager::start(&mut self) -> Result<String>     // noVNC URL
BrowserManager::cdp_ws_url(&self) -> Result<String>    // CDP WebSocket URL
BrowserManager::stop(&mut self)

// 쿠키
extract_cookies(cdp_ws_url: &str) -> Result<HashMap<String, String>>

// 클라이언트
FlipsterClient::new(config: FlipsterConfig) -> Self
FlipsterClient::from_cookies(cookies: &HashMap<String, String>) -> Self
FlipsterClient::reload_cookies(&self, cookies: &HashMap<String, String>)
FlipsterClient::update_cf_bm(&self, new_cf_bm: &str)
FlipsterClient::place_order(&self, symbol, params) -> Result<OrderResponse>
FlipsterClient::close_position(&self, symbol, slot, price) -> Result<OrderResponse>
FlipsterClient::raw_request(&self, symbol, body) -> Result<OrderResponse>

// 주문 빌더
OrderParams::builder()
    .side(Side::Long | Side::Short)
    .price(f64)
    .amount(f64)                        // USD
    .leverage(u32)                      // default 1
    .margin_type(MarginType::Isolated | MarginType::Cross)
    .order_type(OrderType::Market | OrderType::Limit)
    .build()
```

---

## Flipster REST API 명세

### 인증

로그인은 브라우저에서 수동으로 수행. 세션 쿠키를 Chrome DevTools Protocol(CDP)로 추출.

### Headers (모든 요청 공통)

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

### Cookies

| Cookie | Description | Expiry |
|---|---|---|
| `session_id_bolts` | 세션 토큰 A | 로그인 세션 |
| `session_id_nuts` | 세션 토큰 B | 로그인 세션 |
| `ajs_user_id` | 유저 ID | 로그인 세션 |
| `__cf_bm` | **Cloudflare 봇 감지** | **~30분** |
| `_ga` | Google Analytics | 장기 |
| `_ga_RH8FM2JKCM` | GA 세션 | 장기 |
| `ajs_anonymous_id` | 익명 추적 ID | 장기 |
| `analytics_session_id` | 세션 분석 | 세션 |
| `internal` | 내부 플래그 | 세션 |
| `referral_path` | 유입 경로 | 세션 |
| `referrer_symbol` | 참조 심볼 | 세션 |

> `__cf_bm`은 ~30분마다 만료. `refresh_cookies()` / `reload_cookies()`로 갱신 필요.

---

### POST — Open Position

```
POST https://api.flipster.io/api/v2/trade/positions/{symbol}
```

**symbol 예시**: `BTCUSDT.PERP`, `ETHUSDT.PERP`

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

| Field | Type | Description |
|---|---|---|
| `side` | string | `"Long"` 또는 `"Short"` |
| `requestId` | string | UUID v4 (중복 방지) |
| `timestamp` | string | Unix 나노초 (19자리) |
| `refServerTimestamp` | string | 서버 타임스탬프 (현재 클라이언트 시간으로 대체) |
| `refClientTimestamp` | string | 클라이언트 타임스탬프 |
| `leverage` | integer | 레버리지 배수 |
| `price` | string | 참조가 (market) 또는 지정가 (limit) |
| `amount` | string | USD 주문량 |
| `attribution` | string | 항상 `"SEARCH_PERPETUAL"` |
| `marginType` | string | `"Isolated"` 또는 `"Cross"` |
| `orderType` | string | `"ORDER_TYPE_MARKET"` 또는 `"ORDER_TYPE_LIMIT"` |

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
    "achievedFlipComboCount": null,
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
    "action": "Open",
    "triggerPrice": null,
    "triggerDirection": null,
    "triggerTimestamp": null
  },
  "timestamp": "1776322547015644661"
}
```

---

### PUT — Close Position

```
PUT https://api.flipster.io/api/v2/trade/positions/{symbol}/{slot}/size
```

**slot**: `position.slot` (Open 응답에서 획득)

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

| Field | Type | Description |
|---|---|---|
| `size` | string | `"0"` (전량 청산) |
| `price` | string | 현재 참조가 |
| 나머지 | - | Open과 동일 |

---

### Errors

| Status | Error Type | Description |
|---|---|---|
| 200 | - | 성공 |
| 400 | `TooManyPositions` | 슬롯 초과 (심볼당 최대 ~10개) |
| 401 | - | 세션 만료 |
| 403 | - | 접근 거부 / Cloudflare 차단 |

---

## 성능 벤치마크

gate1 서버 기준 (ping 1.6ms to api.flipster.io):

| | Python | Rust (HTTP/2) |
|---|---|---|
| **Open avg** | 120 ms | **79 ms** |
| **Open min** | 87 ms | **64 ms** |
| **Close avg** | 92 ms | **79 ms** |
| **Close min** | 84 ms | **63 ms** |
| **Round trip avg** | 213 ms | **158 ms** |

### 레이턴시 분해 (min ~62ms)

```
Network RTT:     ~3 ms  (ping 1.6ms × 2)
TLS/HTTP/2:      ~2 ms
Server matching: ~57 ms  ← 병목
```

서버 매칭엔진 처리가 92% 차지. 네트워크/대역폭 개선으로 의미 있는 개선 불가.

---

## 주의사항

- **`__cf_bm` 만료**: ~30분. 자동 갱신 안 됨, 명시적 호출 필요
- **`timestamp`**: 나노초 19자리 (`1776319247858000000`)
- **`refServerTimestamp`**: 원래 서버 응답값이지만 클라이언트 시간으로 대체해도 동작. 서버 검증 강화 시 별도 API 필요
- **슬롯 제한**: 심볼당 ~10개. 초과 시 open/close 모두 `TooManyPositions` 에러
- **Cloudflare throttle**: 연속 요청 시 200-300ms 스파이크 발생 가능
- **브라우저 세션 공유**: Python에서 쿠키 추출 → JSON 저장 → Rust에서 로드 가능
