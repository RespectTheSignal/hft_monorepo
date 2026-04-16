# flipster_web

Flipster 거래소 비공식 주문 실행 모듈. 브라우저 세션 쿠키를 이용한 REST API 클라이언트.

> **Warning**: 비공식 reverse-engineered API 기반. Flipster 정책 변경 시 동작하지 않을 수 있음.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  flipster_web (Python)                              │
│                                                     │
│  BrowserManager ──→ Xvfb + Chrome + x11vnc + noVNC  │
│       │                    │                        │
│       │              브라우저에서 로그인               │
│       │                    │                        │
│  CookieExtractor ←── CDP WebSocket ───── Chrome     │
│       │                                             │
│  FlipsterClient ──→ requests ──→ Flipster API       │
│    place_order()     (HTTP/1.1)                     │
│    close_position()                                 │
└─────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────┐
│  flipster-client (Rust)                             │
│                                                     │
│  FlipsterClient ──→ reqwest ──→ Flipster API        │
│    place_order()     (HTTP/2)                       │
│    close_position()                                 │
│                                                     │
│  쿠키를 외부에서 주입 (Python에서 추출 → JSON → Rust) │
└─────────────────────────────────────────────────────┘
```

## 설치

### Python 모듈

```bash
# git clone 후 사용 (PyPI 배포 없음)
git clone <repo-url>
cd hft_monorepo

# 의존성
pip install requests websockets
```

시스템 의존성 (Ubuntu):
```bash
# 필수
sudo apt install xvfb google-chrome-stable

# VNC (sudo 없이 deb 추출로도 가능)
cd /tmp
apt download x11vnc libvncserver1 libvncclient1
dpkg-deb -x x11vnc_*.deb x11vnc-extract
dpkg-deb -x libvncserver1_*.deb x11vnc-extract
dpkg-deb -x libvncclient1_*.deb x11vnc-extract

# noVNC
git clone --depth 1 https://github.com/novnc/noVNC.git ~/.local/share/noVNC
```

### Rust 크레이트

```toml
# Cargo.toml (workspace 멤버로 사용)
[dependencies]
flipster-client = { path = "../flipster-client" }

# 또는 git 직접 참조
flipster-client = { git = "<repo-url>", path = "flipster_kattpish/flipster-client" }
```

## Quick Start (Python)

```python
from flipster_web import FlipsterClient, OrderParams, Side, OrderType, MarginType

# 1. 브라우저 실행
client = FlipsterClient()
url = client.start_browser()
print(f"로그인: {url}")  # http://<ip>:6080/vnc.html

# 2. 브라우저에서 Flipster 로그인 후
client.login_done()

# 3. 주문
result = client.place_order(
    "BTCUSDT.PERP",
    OrderParams(side=Side.LONG, amount_usd=1.0),
    ref_price=75000.0,
)
print(result["order"]["orderId"])

# 4. 청산
slot = result["position"]["slot"]
client.close_position("BTCUSDT.PERP", slot, price=75000.0)

# 5. 종료
client.stop()
```

## API Reference (Python)

### FlipsterClient

| Method | Description |
|---|---|
| `start_browser() → str` | Xvfb + Chrome + VNC 실행, noVNC URL 반환 |
| `login_done()` | Chrome에서 쿠키 추출, HTTP 세션 초기화 |
| `refresh_cookies()` | 쿠키 재추출 (`__cf_bm` 만료 ~30분마다 필요) |
| `place_order(symbol, params, ref_price?) → dict` | 주문 실행 |
| `close_position(symbol, slot, price) → dict` | 포지션 청산 (size=0) |
| `raw_request(symbol, body) → dict` | 디버깅용 raw JSON 전송 |
| `stop()` | 모든 프로세스 종료 |

### OrderParams

```python
@dataclass
class OrderParams:
    side: Side                              # Side.LONG | Side.SHORT
    amount_usd: float                       # USD 기준 주문량
    order_type: OrderType = OrderType.MARKET  # MARKET | LIMIT
    price: float | None = None              # Limit 주문 시 필수
    leverage: int = 1
    margin_type: MarginType = MarginType.ISOLATED  # ISOLATED | CROSS
```

### 주문 예시

```python
# Market Long $1
OrderParams(side=Side.LONG, amount_usd=1.0)

# Market Short $10
OrderParams(side=Side.SHORT, amount_usd=10.0)

# Limit Long $5 @ $70,000, 3x leverage, Cross margin
OrderParams(
    side=Side.LONG,
    amount_usd=5.0,
    order_type=OrderType.LIMIT,
    price=70000.0,
    leverage=3,
    margin_type=MarginType.CROSS,
)
```

### 응답 구조

```json
{
  "position": {
    "symbol": "BTCUSDT.PERP",
    "slot": 0,
    "avgPrice": "74937.1",
    "size": "0.000013",
    "leverage": 1,
    "liqPrice": "18.9",
    "initMargin": "0.9741823",
    "positionId": "6H5+9/HDphg..."
  },
  "order": {
    "orderId": "073d90dc-2b65-...",
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

## Quick Start (Rust)

```rust
use flipster_client::{FlipsterClient, FlipsterConfig, OrderParams, OrderType, Side};

#[tokio::main]
async fn main() {
    // Python에서 추출한 쿠키 로드
    let cookies: HashMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string("/tmp/flipster_cookies.json").unwrap())
        .unwrap();

    let config = FlipsterConfig {
        session_id_bolts: cookies["session_id_bolts"].clone(),
        session_id_nuts: cookies["session_id_nuts"].clone(),
        ajs_user_id: cookies["ajs_user_id"].clone(),
        cf_bm: cookies["__cf_bm"].clone(),
        ga: cookies["_ga"].clone(),
        ga_rh8fm2jkcm: cookies["_ga_RH8FM2JKCM"].clone(),
        ajs_anonymous_id: cookies["ajs_anonymous_id"].clone(),
        analytics_session_id: cookies["analytics_session_id"].clone(),
        internal: cookies["internal"].clone(),
        referral_path: cookies["referral_path"].clone(),
        referrer_symbol: cookies["referrer_symbol"].clone(),
        dry_run: false,
        proxy: None,
    };

    let client = FlipsterClient::new(config);

    // Open
    let r = client.place_order("BTCUSDT.PERP",
        OrderParams::builder()
            .side(Side::Long)
            .price(75000.0)
            .amount(1.0)
            .order_type(OrderType::Market)
            .build()
    ).await.unwrap();

    let slot = r.raw["position"]["slot"].as_u64().unwrap() as u32;

    // Close
    client.close_position("BTCUSDT.PERP", slot, 75000.0).await.unwrap();
}
```

## Performance

gate1 서버 기준 (ping 1.6ms to api.flipster.io):

| | Python (requests) | Rust (reqwest + HTTP/2) |
|---|---|---|
| **Open avg** | 120 ms | **79 ms** |
| **Open min** | 87 ms | **64 ms** |
| **Close avg** | 92 ms | **79 ms** |
| **Close min** | 84 ms | **63 ms** |
| **Round trip avg** | 213 ms | **158 ms** |

- min ~62ms가 한계 (서버 매칭엔진 처리 ~60ms)
- 네트워크/대역폭 개선으로는 의미 있는 개선 불가

## API Endpoints (reverse engineered)

| Action | Method | Endpoint |
|---|---|---|
| Open position | `POST` | `/api/v2/trade/positions/{symbol}` |
| Close position | `PUT` | `/api/v2/trade/positions/{symbol}/{slot}/size` |

## 주의사항

- **`__cf_bm` 쿠키**: Cloudflare 봇 감지 토큰, ~30분마다 만료. `refresh_cookies()` 호출 필요
- **timestamp**: 나노초 단위 (`1776319247858000000`)
- **refServerTimestamp**: 현재는 클라이언트 타임스탬프로 대체. 서버 검증 시 별도 API 필요할 수 있음
- **slot**: Flipster는 같은 심볼에 여러 포지션(slot 0~9)을 가질 수 있음. 청산 시 slot 번호 필수
- **rate limit**: 연속 주문 시 가끔 200-300ms 스파이크 발생 (Cloudflare throttle 추정)

## 파일 구조

```
hft_monorepo/
├── flipster_web/              # Python 모듈
│   ├── __init__.py
│   ├── browser.py             # BrowserManager
│   ├── cookies.py             # CDP 쿠키 추출
│   ├── client.py              # FlipsterClient
│   ├── order.py               # OrderParams, enums
│   └── README.md
│
├── flipster_kattpish/
│   ├── flipster-client/       # Rust 크레이트
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── client.rs      # FlipsterClient
│   │   │   ├── order.rs       # OrderParams, builder
│   │   │   └── error.rs       # FlipsterError
│   │   ├── examples/
│   │   │   └── bench.rs       # 레이턴시 벤치마크
│   │   └── tests/
│   │       └── dry_run.rs     # dry_run 테스트
│   └── ...
```
