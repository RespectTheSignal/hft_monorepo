//! hft-exchange-api — 거래소 feed/executor 의 추상 계약.
//!
//! ## 설계
//!
//! - 각 거래소 구현체는 이 crate 의 trait 만 알고 서비스 레이어와는
//!   독립적으로 컴파일 가능해야 한다. `services/publisher`, `services/order-gateway`
//!   는 **이 trait 만 의존**하며, vendor 구현체는 main 함수에서 주입된다.
//! - `ExchangeFeed::stream` 은 **자체 재연결 + 재구독** 까지 포함하는 장수명 Future.
//!   호출자는 단지 `CancellationToken` 으로 중단 요청만 보낸다.
//! - `Emitter` 는 `Arc<dyn Fn(MarketEvent, LatencyStamps)>` — hot path 에서
//!   실행되므로 **allocation-free** 여야 한다 (캡처된 state 에 Vec / HashMap 접근
//!   같은 건 금지). publisher 쪽 serializer + zmq push 가 이 콜백 안에서 호출된다.
//! - 중간에 channel 을 두지 않는다 — channel 한 단계마다 μs 단위 jitter 가
//!   누적되며 50ms 버짓을 깎아먹는다.
//! - 트레이트 메서드는 전부 `async_trait` 매크로 사용. overhead 는 Box alloc
//!   한 번이지만 장수명 Future 라 무시 가능.
//!
//! ## 계약
//!
//! - `stream(symbols, emit, cancel)` 은:
//!     * 정상 종료 → `Ok(())`
//!     * 복구 불가 오류 (인증, 구성, 바이너리 프로토콜 불일치) → `Err(..)`
//!     * 네트워크/WS 오류 → **내부 재연결**, 호출자에게 전파 금지.
//! - `emit` 는 원하는 만큼 여러 번 호출 가능하며 콜백은 `Send + Sync + 'static`.
//! - `LatencyStamps` 는 feed 가 채운 시점 stamp 를 **얕은 copy** 로 전달
//!   (구조체 value, Arc 없음).
//! - `place_order` 는 idempotent 하지 않다. retry 는 order-gateway 에서
//!   [`OrderRequest::client_id`] 를 dedup 키로 관리.
//!
//! ## Phase 1 TODO
//!
//! 1. `Emitter` 타입을 `Fn(MarketEvent, LatencyStamps)` 로 고정. ✅
//! 2. `ExchangeFeed` 에 `role(&self) -> DataRole` 추가. ✅
//! 3. `stream()` 시그니처에 `cancel: CancellationToken` 추가. ✅
//! 4. `OrderRequest`/`OrderAck` 확장 (exchange, client_id, ts_ms 등). ✅
//! 5. `tokio_util::sync::CancellationToken` 재수출. ✅
//! 6. 트리비얼 `NullFeed` / `NullExecutor` 로 trait object 컴파일 검증. ✅
//!
//! ## Anti-jitter
//!
//! - `Emitter = Arc<dyn Fn(..)>` — 1회의 간접 호출 (vtable). vtable 분기는
//!   CPU 분기 예측으로 상쇄되며, 1M evt/s 수준에서도 무시 가능한 수 ns.
//! - `emit` 내부에서 publisher 가 직접 serialize + push 하므로 큐가 없고
//!   backpressure 는 자연스럽게 WebSocket 수신 루프까지 전파된다.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use hft_time::LatencyStamps;
use hft_types::{DataRole, ExchangeId, MarketEvent, Symbol};

// ────────────────────────────────────────────────────────────────────────────
// 재수출
// ────────────────────────────────────────────────────────────────────────────

/// 협력적 중단 토큰 재수출.
///
/// 구현체/서비스가 `tokio_util` 을 별도로 의존하지 않고도 이 토큰을
/// 바로 쓸 수 있게 한다.
pub use tokio_util::sync::CancellationToken;

// ────────────────────────────────────────────────────────────────────────────
// Emitter
// ────────────────────────────────────────────────────────────────────────────

/// 시장 데이터 이벤트를 hot path 에 방출하는 콜백.
///
/// - `MarketEvent` 는 struct value, `LatencyStamps` 도 value — 콜백 호출
///   자체에 heap allocation 이 발생하지 않도록 한다.
/// - 콜백 안에서 serialize + zmq push 를 직접 수행. 즉 콜백이 blocking
///   될 수 있으며, 이는 WS 수신 루프로의 자연 backpressure 로 설계됨.
pub type Emitter = Arc<dyn Fn(MarketEvent, LatencyStamps) + Send + Sync + 'static>;

/// 빈 emitter — 테스트/미구성 상태에서 안전한 기본값.
///
/// 실제 production 에서 등록되지 않은 emitter 로 이벤트가 흘러들면
/// 데이터 유실이 은밀하게 발생하므로, 호출자는 반드시 자체 emitter 를
/// 주입해야 한다. 이 함수는 테스트/warmup 용 안전망.
pub fn noop_emitter() -> Emitter {
    Arc::new(|_ev, _ls| {})
}

// ────────────────────────────────────────────────────────────────────────────
// ExchangeFeed
// ────────────────────────────────────────────────────────────────────────────

/// 시장 데이터 feed — 특정 거래소의 한 채널(Primary / WebOrderBook)에 대응.
///
/// 같은 거래소에 `Primary` 와 `WebOrderBook` 두 구현체가 동시에 존재할 수 있다
/// (Gate.io 가 대표 예). 두 feed 는 서로 다른 [`DataRole`] 을 반환한다.
#[async_trait]
pub trait ExchangeFeed: Send + Sync {
    /// 거래소 식별자.
    fn id(&self) -> ExchangeId;

    /// 데이터 소스 역할 — `Primary` / `WebOrderBook`.
    fn role(&self) -> DataRole;

    /// 거래 가능한 심볼 목록 조회 (REST).
    ///
    /// - 구독 전 warm-up 단계에서 1회 호출.
    /// - 실패 시 호출자가 재시도/대체 전략을 결정.
    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>>;

    /// WebSocket 구독 + 재연결 루프.
    ///
    /// - `symbols` 가 빈 슬라이스라면 즉시 `Ok(())` 반환 (no-op).
    /// - `emit` 콜백은 메시지 한 개 마다 동기적으로 호출.
    /// - `cancel` 토큰이 trigger 되면 깔끔하게 연결 해제 후 `Ok(())` 반환.
    /// - 내부 재연결 백오프는 구현체 책임 — 권장값: 200ms → 5s exponential.
    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()>;

    /// 사람이 읽는 디버그 이름. 로그 / 메트릭 label 에 사용.
    ///
    /// 기본 구현은 `"{id}:{role}"` 를 돌려준다. 커스텀이 필요하면 override.
    fn label(&self) -> String {
        format!("{}:{:?}", self.id().as_str(), self.role())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Executor
// ────────────────────────────────────────────────────────────────────────────

/// 매수 / 매도.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    /// 문자열 파싱 — 대소문자/별칭 허용.
    ///
    /// - `"buy"`, `"b"`, `"bid"`, `"long"` → Buy
    /// - `"sell"`, `"s"`, `"ask"`, `"short"` → Sell
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "buy" | "b" | "bid" | "long" => Some(Self::Buy),
            "sell" | "s" | "ask" | "short" => Some(Self::Sell),
            _ => None,
        }
    }

    /// 와이어 문자열 — 거래소 API 에 그대로 쓰기 좋은 소문자.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

impl fmt::Display for OrderSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 주문 타입 — Phase 1 은 limit / market 2종만.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    /// Limit — `price` 필수.
    Limit,
    /// Market — `price` 는 무시.
    Market,
}

/// TimeInForce — Phase 1 최소 3종.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    /// Good Till Cancel (default).
    #[default]
    Gtc,
    /// Immediate Or Cancel — 체결 안 되는 수량은 즉시 cancel.
    Ioc,
    /// Fill Or Kill — 전량 즉시 체결 아니면 전체 reject.
    Fok,
}

/// 주문 요청 — `order-gateway` 가 exchange executor 에 전달.
///
/// `client_id` 는 retry 시 중복 주문을 막는 핵심 키이며, 거래소에는
/// `clientOrderId` / `text` / `newClientOrderId` 등으로 주입된다.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderRequest {
    pub exchange: ExchangeId,
    pub symbol: Symbol,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub qty: f64,
    /// `OrderType::Market` 이면 무시.
    pub price: Option<f64>,
    /// 포지션 축소 전용 주문 여부.
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default)]
    pub tif: TimeInForce,
    /// 전략 내부 단조 증가 시퀀스. transport/observability 상관키로 사용한다.
    #[serde(default)]
    pub client_seq: u64,
    /// strategy drain 직전 wall-clock UTC epoch ns.
    #[serde(default)]
    pub origin_ts_ns: u64,
    /// 32-64자 이하 ASCII 권장. 멱등성 보장 키.
    pub client_id: Arc<str>,
}

impl OrderRequest {
    /// 최소한의 필드 validation — `ExchangeExecutor` 구현체는 이 함수를 호출한 뒤
    /// 거래소-특화 검증 (가격 정밀도, 최소 수량 등) 을 덧붙여야 한다.
    pub fn basic_validate(&self) -> Result<(), ApiError> {
        if !self.qty.is_finite() || self.qty <= 0.0 {
            return Err(ApiError::InvalidOrder("qty 는 양수여야 함".into()));
        }
        if matches!(self.order_type, OrderType::Limit) {
            match self.price {
                Some(p) if p.is_finite() && p > 0.0 => {}
                _ => return Err(ApiError::InvalidOrder("limit 주문은 양의 price 필요".into())),
            }
        }
        if self.client_id.is_empty() {
            return Err(ApiError::InvalidOrder("client_id 는 비어있을 수 없음".into()));
        }
        if self.symbol.as_str().is_empty() {
            return Err(ApiError::InvalidOrder("symbol 은 비어있을 수 없음".into()));
        }
        Ok(())
    }
}

/// 주문 접수 확인 — 거래소가 주문을 받고 ID 를 발급한 시점.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderAck {
    pub exchange: ExchangeId,
    /// 거래소가 발급한 고유 ID (문자열 보존 — 거래소 format 존중).
    pub exchange_order_id: String,
    /// 우리가 보낸 client_id 그대로 echo.
    pub client_id: Arc<str>,
    /// 거래소 수신 시각 (wall-clock ms). 응답에 없으면 현재 시각.
    pub ts_ms: i64,
}

/// 주문 실행 게이트웨이 — 특정 거래소에 대한 실행 권한 holder.
#[async_trait]
pub trait ExchangeExecutor: Send + Sync {
    /// 거래소 식별자.
    fn id(&self) -> ExchangeId;

    /// 주문 요청. 멱등성 여부는 구현체 문서 참조.
    ///
    /// ## 오류 처리
    /// - 네트워크 일시 오류는 구현체 내부에서 **짧게** 재시도 (최대 3회)
    /// - rate limit 이면 [`ApiError::RateLimited`] 로 반환
    /// - 거래소가 명시적으로 reject 하면 [`ApiError::Rejected`]
    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck>;

    /// 주문 취소. 이미 체결/취소된 ID 도 멱등 성공(`Ok(())`) 으로 간주 가능.
    async fn cancel(&self, exchange_order_id: &str) -> anyhow::Result<()>;

    /// 디버그 라벨 — 기본은 `id` 를 그대로 사용.
    fn label(&self) -> String {
        self.id().as_str().to_string()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 에러
// ────────────────────────────────────────────────────────────────────────────

/// 공통 거래소 오류 — 구현체가 anyhow 로 감쌀 때 원인을 구분할 수 있게 한다.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("auth failed: {0}")]
    Auth(String),

    #[error("rate limited (retry after {retry_after_ms} ms)")]
    RateLimited { retry_after_ms: u64 },

    #[error("order rejected by exchange: {0}")]
    Rejected(String),

    #[error("invalid order: {0}")]
    InvalidOrder(String),

    #[error("connection error: {0}")]
    Connection(String),

    #[error("protocol decode error: {0}")]
    Decode(String),

    #[error("cancelled")]
    Cancelled,
}

// ────────────────────────────────────────────────────────────────────────────
// FeedState — 재연결 상태 머신 (선택적)
// ────────────────────────────────────────────────────────────────────────────

/// WebSocket feed 의 상위 상태.
///
/// 구현체는 이 enum 을 내부에서 돌려써도 되고, 커스텀 상태를 만들어도 된다.
/// 목적은 observability 쪽에서 "지금 feed 가 어떤 단계인가" 를 문자열로
/// 찍을 수 있게 하는 것.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeedState {
    /// 초기화 완료, 아직 연결 전.
    Idle,
    /// TCP + WS handshake 중.
    Connecting,
    /// subscribe 메시지 전송, 응답 대기.
    Subscribing,
    /// 정상 데이터 수신 중.
    Streaming,
    /// backoff 중 (재연결 대기).
    Backoff,
    /// `cancel` 수신 → cleanup.
    Cancelling,
    /// 최종 종료.
    Closed,
}

impl FeedState {
    /// 메트릭/로그 용 소문자 라벨.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Subscribing => "subscribing",
            Self::Streaming => "streaming",
            Self::Backoff => "backoff",
            Self::Cancelling => "cancelling",
            Self::Closed => "closed",
        }
    }

    /// 현재 상태가 "건강한" 구독 상태인가?
    pub const fn is_healthy(&self) -> bool {
        matches!(self, Self::Streaming)
    }
}

impl fmt::Display for FeedState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Backoff — 재연결 지연 계산 헬퍼
// ────────────────────────────────────────────────────────────────────────────

/// 재연결 지수 백오프. 구현체는 이 타입을 보유해 연속 실패 횟수에 따라
/// 대기 시간을 계산한다. jitter 추가 없음 — 단일 프로세스 내 소수 feed 라
/// collision 가 실질적 문제되지 않음. 필요하면 call-site 에서 rand 로 섞을 것.
///
/// ```
/// use hft_exchange_api::Backoff;
/// let mut b = Backoff::new_defaults();
/// assert_eq!(b.next_ms(), 200);
/// assert_eq!(b.next_ms(), 400);
/// assert_eq!(b.next_ms(), 800);
/// b.reset();
/// assert_eq!(b.next_ms(), 200);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    base_ms: u64,
    cap_ms: u64,
    attempt: u32,
}

impl Backoff {
    /// `base_ms` 부터 2^n 배로 증가하며 `cap_ms` 에서 포화.
    pub const fn new(base_ms: u64, cap_ms: u64) -> Self {
        Self { base_ms, cap_ms, attempt: 0 }
    }

    /// 200ms 시작, 5000ms 상한 — WebSocket feed 에 적당한 기본값.
    pub const fn new_defaults() -> Self {
        Self::new(200, 5_000)
    }

    /// 다음 지연값(ms) 을 계산하고 내부 카운터 증가.
    pub fn next_ms(&mut self) -> u64 {
        // shift 가 overflow 하지 않도록 상한을 일찍 건다.
        let shift = self.attempt.min(20);
        self.attempt = self.attempt.saturating_add(1);
        let delay = self.base_ms.saturating_mul(1u64 << shift);
        delay.min(self.cap_ms)
    }

    /// 성공적 연결 시 호출 — 카운터 리셋.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// 현재까지 연속 실패 횟수.
    pub const fn attempts(&self) -> u32 {
        self.attempt
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── 트리비얼 구현 — trait object 가 실제로 컴파일되는지 검증.

    struct NullFeed;

    #[async_trait]
    impl ExchangeFeed for NullFeed {
        fn id(&self) -> ExchangeId {
            ExchangeId::Gate
        }
        fn role(&self) -> DataRole {
            DataRole::Primary
        }
        async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
            Ok(vec![Symbol::new("BTC_USDT")])
        }
        async fn stream(
            &self,
            _symbols: &[Symbol],
            _emit: Emitter,
            cancel: CancellationToken,
        ) -> anyhow::Result<()> {
            cancel.cancelled().await;
            Ok(())
        }
    }

    struct NullExec;

    #[async_trait]
    impl ExchangeExecutor for NullExec {
        fn id(&self) -> ExchangeId {
            ExchangeId::Gate
        }
        async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck> {
            req.basic_validate()?;
            Ok(OrderAck {
                exchange: req.exchange,
                exchange_order_id: "null-1".into(),
                client_id: req.client_id.clone(),
                ts_ms: 0,
            })
        }
        async fn cancel(&self, _id: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn traits_are_object_safe() {
        // trait object 로 저장 가능해야 서비스 레이어에서 Arc<dyn ...> 로 주입 가능.
        let _feed: Arc<dyn ExchangeFeed> = Arc::new(NullFeed);
        let _exec: Arc<dyn ExchangeExecutor> = Arc::new(NullExec);
    }

    #[test]
    fn order_side_parse_and_display() {
        assert_eq!(OrderSide::parse("buy"), Some(OrderSide::Buy));
        assert_eq!(OrderSide::parse("B"), Some(OrderSide::Buy));
        assert_eq!(OrderSide::parse("bid"), Some(OrderSide::Buy));
        assert_eq!(OrderSide::parse("long"), Some(OrderSide::Buy));
        assert_eq!(OrderSide::parse("sell"), Some(OrderSide::Sell));
        assert_eq!(OrderSide::parse("ASK"), Some(OrderSide::Sell));
        assert_eq!(OrderSide::parse("xxx"), None);
        assert_eq!(format!("{}", OrderSide::Buy), "buy");
        assert_eq!(format!("{}", OrderSide::Sell), "sell");
    }

    #[test]
    fn order_request_basic_validate_rejects_bad_inputs() {
        let base = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 1.0,
            price: Some(100.0),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("c1"),
        };
        assert!(base.basic_validate().is_ok());

        let mut bad = base.clone();
        bad.qty = 0.0;
        assert!(bad.basic_validate().is_err());

        let mut bad = base.clone();
        bad.qty = f64::NAN;
        assert!(bad.basic_validate().is_err());

        let mut bad = base.clone();
        bad.price = None;
        assert!(bad.basic_validate().is_err());

        let mut bad = base.clone();
        bad.order_type = OrderType::Market;
        bad.price = None;
        // market 은 price 없어도 OK.
        assert!(bad.basic_validate().is_ok());

        let mut bad = base.clone();
        bad.client_id = Arc::from("");
        assert!(bad.basic_validate().is_err());
    }

    #[test]
    fn noop_emitter_is_callable() {
        let e = noop_emitter();
        let ev = MarketEvent::BookTicker(hft_types::BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: hft_types::Price(100.0),
            ask_price: hft_types::Price(100.1),
            bid_size: hft_types::Size(1.0),
            ask_size: hft_types::Size(1.0),
            event_time_ms: 0,
            server_time_ms: 0,
        });
        let ls = LatencyStamps::new();
        // panic 안 하면 success.
        (e)(ev, ls);
    }

    #[tokio::test]
    async fn null_feed_exits_on_cancel() {
        let feed = NullFeed;
        let token = CancellationToken::new();
        let t2 = token.clone();

        let handle = tokio::spawn(async move {
            feed.stream(&[Symbol::new("BTC_USDT")], noop_emitter(), t2).await
        });

        // 잠깐 대기 후 cancel.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        token.cancel();

        let res = tokio::time::timeout(std::time::Duration::from_millis(200), handle)
            .await
            .expect("task did not complete on cancel")
            .expect("task panicked");
        assert!(res.is_ok(), "stream must return Ok on cancel: {:?}", res.err());
    }

    #[tokio::test]
    async fn emitter_actually_invokes_shared_callback() {
        // 사용자 emitter 가 실제 공유 state 에 쓸 수 있는지 검증 — `Send + Sync + 'static`.
        static COUNT: AtomicUsize = AtomicUsize::new(0);

        let emit: Emitter = Arc::new(|_ev, _ls| {
            COUNT.fetch_add(1, Ordering::SeqCst);
        });

        let ev = MarketEvent::BookTicker(hft_types::BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: hft_types::Price(1.0),
            ask_price: hft_types::Price(1.1),
            bid_size: hft_types::Size(1.0),
            ask_size: hft_types::Size(1.0),
            event_time_ms: 0,
            server_time_ms: 0,
        });
        for _ in 0..5 {
            (emit)(ev.clone(), LatencyStamps::new());
        }
        assert_eq!(COUNT.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn null_exec_roundtrip() {
        let exec: Arc<dyn ExchangeExecutor> = Arc::new(NullExec);
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            qty: 1.0,
            price: None,
            reduce_only: false,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("abc"),
        };
        let ack = exec.place_order(req.clone()).await.unwrap();
        assert_eq!(ack.client_id.as_ref(), "abc");
        assert_eq!(ack.exchange, ExchangeId::Gate);
        assert_eq!(ack.exchange_order_id, "null-1");
        exec.cancel("null-1").await.unwrap();
    }

    #[test]
    fn feed_state_is_healthy_only_for_streaming() {
        for s in [
            FeedState::Idle,
            FeedState::Connecting,
            FeedState::Subscribing,
            FeedState::Backoff,
            FeedState::Cancelling,
            FeedState::Closed,
        ] {
            assert!(!s.is_healthy(), "{:?} must not be healthy", s);
        }
        assert!(FeedState::Streaming.is_healthy());
        assert_eq!(FeedState::Streaming.label(), "streaming");
    }

    #[test]
    fn backoff_progression_saturates_at_cap() {
        let mut b = Backoff::new(100, 1_000);
        assert_eq!(b.next_ms(), 100);
        assert_eq!(b.next_ms(), 200);
        assert_eq!(b.next_ms(), 400);
        assert_eq!(b.next_ms(), 800);
        assert_eq!(b.next_ms(), 1_000); // cap
        assert_eq!(b.next_ms(), 1_000); // 여전히 cap
        assert!(b.attempts() >= 6);
        b.reset();
        assert_eq!(b.attempts(), 0);
        assert_eq!(b.next_ms(), 100);
    }

    #[test]
    fn backoff_large_attempt_count_does_not_overflow() {
        let mut b = Backoff::new(500, 30_000);
        for _ in 0..1000 {
            let d = b.next_ms();
            assert!(d <= 30_000);
        }
    }

    #[test]
    fn api_error_display() {
        let e = ApiError::RateLimited { retry_after_ms: 250 };
        assert!(format!("{e}").contains("rate limited"));
        let e = ApiError::Rejected("insufficient balance".into());
        assert!(format!("{e}").contains("insufficient"));
    }

    #[test]
    fn label_default_formats_id_and_role() {
        let feed = NullFeed;
        assert_eq!(feed.label(), "gate:Primary");
    }

    #[test]
    fn serde_roundtrip_order_side_and_type() {
        let s = OrderSide::Buy;
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(j, "\"buy\"");
        let t = OrderType::Market;
        let j = serde_json::to_string(&t).unwrap();
        assert_eq!(j, "\"market\"");
        let tif = TimeInForce::Ioc;
        let j = serde_json::to_string(&tif).unwrap();
        assert_eq!(j, "\"IOC\"");
    }
}
