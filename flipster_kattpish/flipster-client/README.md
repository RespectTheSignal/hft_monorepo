# flipster-client (Rust)

Flipster 거래소 비공식 주문 실행 크레이트. 순수 Rust로 브라우저 관리, 쿠키 추출, 주문 실행까지 올인원.

> **Warning**: reverse-engineered 브라우저 API 기반. 공식 API 아님.

## 설치

```toml
# 같은 workspace 내
[dependencies]
flipster-client = { path = "../flipster-client" }

# 또는 git
[dependencies]
flipster-client = { git = "https://github.com/RespectTheSignal/hft_monorepo.git", path = "flipster_kattpish/flipster-client" }
```

### 시스템 의존성

Python 버전과 동일 (Xvfb, Chrome, x11vnc, noVNC). [flipster_web/README.md](../../flipster_web/README.md#시스템-의존성-ubuntu) 참조.

## Quick Start

```rust
use flipster_client::{BrowserManager, FlipsterClient, OrderParams, OrderType, Side};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 브라우저 실행 (Xvfb + Chrome + VNC)
    let mut browser = BrowserManager::new();
    let url = browser.start()?;
    println!("로그인: {url}");  // http://0.0.0.0:6080/vnc.html

    // 2. 로그인 완료 후 쿠키 추출
    let ws_url = browser.cdp_ws_url().await?;
    let cookies = flipster_client::extract_cookies(&ws_url).await?;

    // 3. 클라이언트 생성
    let client = FlipsterClient::from_cookies(&cookies);

    // 4. 주문
    let result = client.place_order("BTCUSDT.PERP",
        OrderParams::builder()
            .side(Side::Long)
            .price(75000.0)
            .amount(1.0)
            .order_type(OrderType::Market)
            .build()
    ).await?;

    let slot = result.raw["position"]["slot"].as_u64().unwrap() as u32;
    println!("Opened slot {slot}");

    // 5. 청산
    client.close_position("BTCUSDT.PERP", slot, 75000.0).await?;

    // 6. 종료 (Drop이 자동 정리하지만 명시적으로도 가능)
    browser.stop();
    Ok(())
}
```

## 쿠키 갱신 (~30분마다)

```rust
// 방법 1: CDP에서 전체 쿠키 재추출
let cookies = flipster_client::extract_cookies(&ws_url).await?;
client.reload_cookies(&cookies);

// 방법 2: __cf_bm만 업데이트
client.update_cf_bm("new_cf_bm_value").await;
```

---

## API Reference

### BrowserManager

```rust
impl BrowserManager {
    fn new() -> Self;
    fn start(&mut self) -> Result<String, FlipsterError>;  // noVNC URL 반환
    async fn cdp_ws_url(&self) -> Result<String, FlipsterError>;
    fn stop(&mut self);
}
```

### extract_cookies

```rust
/// CDP WebSocket으로 Chrome에서 Flipster 쿠키 추출
async fn extract_cookies(cdp_ws_url: &str) -> Result<HashMap<String, String>, FlipsterError>;
```

### FlipsterClient

```rust
impl FlipsterClient {
    fn new(config: FlipsterConfig) -> Self;
    fn from_cookies(cookies: &HashMap<String, String>) -> Self;
    fn reload_cookies(&self, cookies: &HashMap<String, String>);
    async fn update_cf_bm(&self, new_cf_bm: &str);
    async fn place_order(&self, symbol: &str, params: OrderParams) -> Result<OrderResponse, FlipsterError>;
    async fn close_position(&self, symbol: &str, slot: u32, price: f64) -> Result<OrderResponse, FlipsterError>;
    async fn raw_request(&self, symbol: &str, body: Value) -> Result<OrderResponse, FlipsterError>;
}
```

### FlipsterConfig

```rust
pub struct FlipsterConfig {
    pub session_id_bolts: String,
    pub session_id_nuts: String,
    pub ajs_user_id: String,
    pub cf_bm: String,
    pub ga: String,
    pub ga_rh8fm2jkcm: String,
    pub ajs_anonymous_id: String,
    pub analytics_session_id: String,
    pub internal: String,
    pub referral_path: String,
    pub referrer_symbol: String,
    pub dry_run: bool,
    pub proxy: Option<String>,
}
```

### OrderParams (builder)

```rust
let order = OrderParams::builder()
    .side(Side::Long)           // Long | Short
    .price(75000.0)             // 참조 가격 (market), 지정가 (limit)
    .amount(1.0)                // USD
    .leverage(1)                // default 1
    .margin_type(MarginType::Isolated)  // Isolated | Cross
    .order_type(OrderType::Market)      // Market | Limit
    .build();
```

### OrderResponse

```rust
pub struct OrderResponse {
    pub raw: serde_json::Value,  // 전체 API 응답
}
// result.raw["position"]["slot"], result.raw["order"]["orderId"] 등으로 접근
```

### FlipsterError

```rust
pub enum FlipsterError {
    Http(reqwest::Error),                    // 네트워크 에러
    Api { status: u16, message: String },    // API 에러 (400, 500 등)
    Auth,                                     // 401/403 세션 만료
}
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

| | Python (requests) | **Rust (reqwest + HTTP/2)** |
|---|---|---|
| Open avg | 120 ms | **79 ms** |
| Open min | 87 ms | **64 ms** |
| Close avg | 92 ms | **79 ms** |
| Close min | 84 ms | **63 ms** |
| Round trip avg | 213 ms | **158 ms** |

min ~62ms = 서버 매칭엔진 처리 한계. 네트워크/대역폭 개선으로 의미 있는 개선 불가.

---

## 주의사항

- `__cf_bm` 쿠키 ~30분 만료 → `reload_cookies()` 또는 `update_cf_bm()` 필요
- `timestamp` 나노초 단위 (19자리). `chrono::Utc::now().timestamp_nanos_opt()` 사용
- `refServerTimestamp` — 현재 클라이언트 타임스탬프로 대체. 서버 검증 시 별도 API 필요할 수 있음
- 심볼당 슬롯 최대 ~10개. 초과 시 `TooManyPositions` 에러
- `dry_run: true` 설정 시 HTTP 전송 없이 request body만 반환

## 파일 구조

```
flipster-client/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs          # re-exports
│   ├── browser.rs      # BrowserManager (Xvfb + Chrome + x11vnc + noVNC)
│   ├── cookies.rs      # CDP WebSocket 쿠키 추출
│   ├── client.rs       # FlipsterClient + FlipsterConfig
│   ├── order.rs        # OrderParams (builder), enums
│   └── error.rs        # FlipsterError
├── examples/
│   └── bench.rs        # 레이턴시 벤치마크
└── tests/
    └── dry_run.rs      # dry_run 모드 테스트
```
