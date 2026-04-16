//! order-gateway — strategy 의 `OrderRequest` 를 거래소 Executor 에 routing.
//!
//! # 역할 (SPEC.md)
//! - strategy 로부터 `crossbeam_channel::Receiver<OrderRequest>` 를 받아 소비.
//! - 요청 내 `OrderRequest::exchange` 에 따라 미리 등록된 `ExchangeExecutor` 로 dispatch.
//! - `client_id` dedup (idempotency): 최근 N개 캐시. 동일 client_id 가 반복되면 재전송 생략.
//! - 네트워크/일시 오류는 exponential backoff 최대 3회. rejected 는 즉시 포기.
//! - ack 는 optional `Sender<OrderAck>` 로 전달 — subscriber/strategy 또는 storage 가 소비.
//!
//! # 핫패스 아님
//! 주문은 ms 단위의 지연이 허용되는 경로라 async runtime (기본 multi-thread) 에
//! `tokio::spawn` 한 단일 router task 가 loop 로 처리. retry 시 `tokio::time::sleep`
//! 도 허용 — strategy 채널을 블로킹하지 않는다.
//!
//! # 범위
//! - `Router` + `NoopExecutor` — NoopExecutor 는 dev/dry-run 시 credential 이
//!   주입되지 않은 거래소에 대한 fallback 역할 (Phase 2 이후 유지).
//! - 실 executor (Gate/Binance/Bybit/Bitget/OKX) 는 `order-gateway` 바이너리의
//!   `main.rs::load_executor()` 가 env var 에서 자격을 읽어 구성.
//! - `RetryPolicy` 파라미터 + 테스트.
//! - dedup cache: `Arc<Mutex<AHashSet<Arc<str>>>>` 로 간단히 구현 (핫패스 아님).
//! - Go IPC route 자리만 `GoIpcRoute` enum variant 로 노출, body 는 아직 unimplemented.
//!
//! # SHM ingestion (Phase 2 Track C)
//! Python strategy 가 `/dev/shm/hft_orders_v2` 에 `OrderFrame` 을 push 하면
//! [`shm_sub::ShmOrderSubscriber`] 가 dedicated OS thread 로 그것을 소비해
//! Place 는 `req_tx`, Cancel 은 `RoutingTable` 를 통해 직접 executor 로 dispatch 한다.
//! 자세한 내용은 [`shm_sub`] 참고.
//!
//! # ZMQ ingestion (Phase 2 E Step 5)
//! Rust strategy 가 `OrderRequestWire` 128B frame 을 PUSH 하면
//! [`zmq_ingress`] 가 PULL socket 으로 수신해 [`OrderRequest`] 로 정규화한 뒤
//! 같은 `req_tx` 로 fan-in 한다. SHM/ZMQ ingress 모두 Router 는 동일 구현을 재사용한다.

#![deny(rust_2018_idioms)]

/// Python strategy → Rust order-gateway 주문 수신 (SHM ingress).
pub mod shm_sub;
/// Strategy ZMQ PUSH → Rust order-gateway raw wire ingress.
pub mod zmq_ingress;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use hft_exchange_api::{CancellationToken, ExchangeExecutor, OrderAck, OrderRequest};
use hft_protocol::order_wire::{OrderResultWire, STATUS_ACCEPTED, STATUS_REJECTED};
use hft_telemetry::{counter_inc, CounterKey};
use hft_types::ExchangeId;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// dedup 캐시 기본 크기. 너무 크면 메모리, 너무 작으면 retry 가 dedup 을 뚫을 위험.
pub const DEDUP_CACHE_CAP_DEFAULT: usize = 4096;

/// retry 기본 값 (SPEC: max 3회, base 100ms, cap 2s).
pub const RETRY_MAX_DEFAULT: u32 = 3;
pub const RETRY_BASE_MS_DEFAULT: u64 = 100;
pub const RETRY_CAP_MS_DEFAULT: u64 = 2_000;

/// ingress 구현이 router 로 함께 전달하는 gateway-local 메타데이터.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngressMeta {
    /// SHM path 인 경우 origin strategy VM id. ZMQ path 는 None.
    pub origin_vm_id: Option<u32>,
    /// request wire/aux 에서 복원한 strategy text_tag.
    pub text_tag: [u8; 32],
}

/// router 내부 채널 payload.
pub type IngressEnvelope = (OrderRequest, IngressMeta);

// ─────────────────────────────────────────────────────────────────────────────
// Retry policy
// ─────────────────────────────────────────────────────────────────────────────

/// 재시도 정책. exponential backoff, ±jitter 없음 (Phase 1 는 단순).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 최대 재시도 횟수 (초기 시도는 제외).
    pub max_attempts: u32,
    /// 첫 backoff (ms).
    pub base_ms: u64,
    /// 최대 backoff (ms).
    pub cap_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: RETRY_MAX_DEFAULT,
            base_ms: RETRY_BASE_MS_DEFAULT,
            cap_ms: RETRY_CAP_MS_DEFAULT,
        }
    }
}

impl RetryPolicy {
    /// `attempt` 는 1부터. (`base_ms * 2^(attempt-1)`).clamp(cap_ms).
    pub fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(20); // 2^20 이상은 무의미
        let raw = self.base_ms.saturating_mul(1u64 << shift);
        Duration::from_millis(raw.min(self.cap_ms))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Dedup cache (idempotency)
// ─────────────────────────────────────────────────────────────────────────────

/// 최근 처리한 `client_id` 캐시. FIFO 제한 크기.
///
/// 핫패스가 아니라 `Mutex<VecDeque>` 로 충분. 캐시 HIT 은 주문을 skip 하고 ack-skip 로그.
pub struct DedupCache {
    inner: parking_lot_free::Mutex<VecDeque<Arc<str>>>,
    cap: usize,
}

// `parking_lot_free` 별칭 — 외부 의존성을 줄이기 위해 std Mutex 를 재export.
// Phase 2 에서 parking_lot 으로 교체 권장.
mod parking_lot_free {
    //! std::sync::Mutex 래퍼 — 다른 crate 에 parking_lot 의존성을 전염시키지 않음.
    pub use std::sync::Mutex;
}

impl DedupCache {
    /// 새 캐시.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: parking_lot_free::Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }

    /// `client_id` 가 이미 처리되었는지 확인. 없으면 삽입하고 `false` 반환.
    /// 있으면 `true` 반환 (caller skip).
    pub fn check_and_insert(&self, cid: &Arc<str>) -> bool {
        // poisoned mutex 는 expect — order-gateway 경로 panic 은 프로세스 종료 가치가 있음.
        let mut q = self.inner.lock().expect("dedup mutex poisoned");
        if q.iter().any(|c| c.as_ref() == cid.as_ref()) {
            return true;
        }
        if q.len() == self.cap {
            q.pop_front();
        }
        q.push_back(cid.clone());
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Routes
// ─────────────────────────────────────────────────────────────────────────────

/// 거래소별 routing 규칙. 실제 rust executor 또는 Go IPC (future) 로 전달.
pub enum Route {
    /// Rust 측 `ExchangeExecutor` 구현체.
    Rust(Arc<dyn ExchangeExecutor>),
    /// Go `order-processor` 서버로 IPC. Phase 1 에선 endpoint 만 보유하고 실제 send 는
    /// unimplemented → 호출 시 에러.
    GoIpc {
        /// ZMQ REQ 소켓이 붙을 endpoint (`tcp://...` 등).
        endpoint: String,
    },
}

impl Route {
    /// route 라벨 (metric/log).
    pub fn label(&self) -> String {
        match self {
            Self::Rust(e) => format!("rust:{}", e.label()),
            Self::GoIpc { endpoint } => format!("go:{endpoint}"),
        }
    }
}

/// 거래소 → Route 매핑.
#[derive(Default)]
pub struct RoutingTable {
    map: HashMap<ExchangeId, Route>,
}

impl RoutingTable {
    /// 새 빈 테이블.
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// route 등록. 동일 id 가 이미 있으면 덮어쓴다 (log warn).
    pub fn insert(&mut self, id: ExchangeId, route: Route) {
        if let Some(prev) = self.map.insert(id, route) {
            warn!(
                target: "order_gateway::routing",
                exchange = ?id,
                replaced_route = %prev.label(),
                "routing entry replaced"
            );
        }
    }

    /// 조회.
    pub fn get(&self, id: ExchangeId) -> Option<&Route> {
        self.map.get(&id)
    }

    /// 등록된 route 수.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 비어있는지.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// NoopExecutor — Phase 1 실 executor 대체
// ─────────────────────────────────────────────────────────────────────────────

/// 모든 주문에 즉시 fake ack 로 응답. 테스트/통합 시나리오용.
pub struct NoopExecutor {
    id: ExchangeId,
}

impl NoopExecutor {
    /// 새 Noop.
    pub fn new(id: ExchangeId) -> Self {
        Self { id }
    }
}

#[async_trait]
impl ExchangeExecutor for NoopExecutor {
    fn id(&self) -> ExchangeId {
        self.id
    }

    async fn place_order(&self, req: OrderRequest) -> Result<OrderAck> {
        debug!(
            target: "order-gateway::noop",
            exchange = %self.id.as_str(),
            symbol = req.symbol.as_str(),
            reduce_only = req.reduce_only,
            "noop executor accepted order"
        );
        Ok(OrderAck {
            exchange: self.id,
            exchange_order_id: format!("noop-{}", req.client_id),
            client_id: req.client_id,
            ts_ms: 0,
        })
    }

    async fn cancel(&self, _exchange_order_id: &str) -> Result<()> {
        Ok(())
    }

    fn label(&self) -> String {
        format!("noop:{}", self.id.as_str())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Router task
// ─────────────────────────────────────────────────────────────────────────────

/// order-gateway router. 채널 `rx` 에서 `OrderRequest` 를 꺼내 dispatch.
pub struct Router {
    routing: Arc<RoutingTable>,
    rx: Receiver<IngressEnvelope>,
    ack_tx: Option<Sender<OrderAck>>,
    result_tx: Option<Sender<OrderResultWire>>,
    dedup: Arc<DedupCache>,
    retry: RetryPolicy,
}

impl Router {
    /// 새 Router.
    pub fn new(
        routing: Arc<RoutingTable>,
        rx: Receiver<IngressEnvelope>,
        ack_tx: Option<Sender<OrderAck>>,
        result_tx: Option<Sender<OrderResultWire>>,
        dedup: Arc<DedupCache>,
        retry: RetryPolicy,
    ) -> Self {
        Self {
            routing,
            rx,
            ack_tx,
            result_tx,
            dedup,
            retry,
        }
    }

    /// async run — `cancel` 이 trip 되면 즉시 break.
    /// 진행 중이던 place_order 요청은 abort 되지 않고 끝까지 기다림(무시 X).
    pub async fn run(self, cancel: CancellationToken) {
        info!(
            target: "order_gateway::router",
            routes = self.routing.len(),
            "order gateway router starting"
        );

        // self 를 async loop 안에서 필드 분리 사용. clone 은 Arc / Receiver 뿐.
        let Router {
            routing,
            rx,
            ack_tx,
            result_tx,
            dedup,
            retry,
        } = self;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok((req, meta)) => {
                    handle_request(
                        &routing,
                        &dedup,
                        &retry,
                        ack_tx.as_ref(),
                        result_tx.as_ref(),
                        req,
                        meta,
                    )
                    .await;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    tokio::task::yield_now().await;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    info!(
                        target: "order_gateway::router",
                        "order channel disconnected — exiting"
                    );
                    break;
                }
            }
        }

        info!(target: "order_gateway::router", "order gateway router stopped");
    }
}

/// 한 request 를 처리. dedup check → route 조회 → place_order + retry → ack forward.
async fn handle_request(
    routing: &RoutingTable,
    dedup: &DedupCache,
    retry: &RetryPolicy,
    ack_tx: Option<&Sender<OrderAck>>,
    result_tx: Option<&Sender<OrderResultWire>>,
    req: OrderRequest,
    meta: IngressMeta,
) {
    if let Err(e) = req.basic_validate() {
        warn!(
            target: "order_gateway::router",
            client_id = %req.client_id,
            error = %e,
            "order rejected at validation"
        );
        counter_inc(CounterKey::SerializationFail);
        emit_result_wire(result_tx, &req, &meta, None, STATUS_REJECTED);
        return;
    }

    if dedup.check_and_insert(&req.client_id) {
        debug!(
            target: "order_gateway::router",
            client_id = %req.client_id,
            "duplicate client_id — skipping"
        );
        return;
    }

    let Some(route) = routing.get(req.exchange) else {
        warn!(
            target: "order_gateway::router",
            exchange = ?req.exchange,
            client_id = %req.client_id,
            "no route for exchange — dropping order"
        );
        counter_inc(CounterKey::OrderGatewayExchangeNotFound);
        emit_result_wire(result_tx, &req, &meta, None, STATUS_REJECTED);
        return;
    };

    let ack = match route {
        Route::Rust(exec) => place_with_retry(exec.as_ref(), &req, retry).await,
        Route::GoIpc { endpoint } => {
            error!(
                target: "order_gateway::router",
                endpoint = %endpoint,
                client_id = %req.client_id,
                "Go IPC route is not implemented in Phase 1"
            );
            counter_inc(CounterKey::ZmqDropped);
            emit_result_wire(result_tx, &req, &meta, None, STATUS_REJECTED);
            return;
        }
    };

    match ack {
        Ok(a) => {
            if let Some(tx) = ack_tx {
                if let Err(TrySendError::Full(_)) = tx.try_send(a.clone()) {
                    counter_inc(CounterKey::ZmqDropped);
                    warn!(
                        target: "order_gateway::router",
                        client_id = %a.client_id,
                        "ack channel full — dropping ack"
                    );
                }
            }
            emit_result_wire(result_tx, &req, &meta, Some(&a), STATUS_ACCEPTED);
            counter_inc(CounterKey::OrderGatewayRoutedOk);
            counter_inc(CounterKey::PipelineEvent);
        }
        Err(e) => {
            counter_inc(CounterKey::ZmqDropped);
            emit_result_wire(result_tx, &req, &meta, None, STATUS_REJECTED);
            error!(
                target: "order_gateway::router",
                client_id = %req.client_id,
                error = %e,
                "place_order failed after retries"
            );
        }
    }
}

/// `place_order` 를 retry policy 에 따라 재시도.
///
/// 첫 시도 + `max_attempts` 재시도 = 최대 `max_attempts + 1` 회 호출.
async fn place_with_retry(
    exec: &dyn ExchangeExecutor,
    req: &OrderRequest,
    retry: &RetryPolicy,
) -> Result<OrderAck> {
    let mut last_err: Option<anyhow::Error> = None;
    let total_attempts = retry.max_attempts.saturating_add(1);

    for attempt in 1..=total_attempts {
        match exec.place_order(req.clone()).await {
            Ok(ack) => {
                if attempt > 1 {
                    info!(
                        target: "order_gateway::router",
                        client_id = %req.client_id,
                        attempt = attempt,
                        "place_order succeeded after retry"
                    );
                }
                return Ok(ack);
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < total_attempts {
                    // attempt 는 1-based; retry.backoff(attempt) 는 attempt 번째 호출 이후 대기 시간.
                    let wait = retry.backoff(attempt);
                    debug!(
                        target: "order_gateway::router",
                        client_id = %req.client_id,
                        attempt = attempt,
                        wait_ms = wait.as_millis() as u64,
                        "place_order error — backing off"
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("place_order failed (no error captured)")))
}

fn wall_clock_epoch_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn encode_zero_padded<const N: usize>(value: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = value.as_bytes();
    let len = bytes.len().min(N);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

fn emit_result_wire(
    result_tx: Option<&Sender<OrderResultWire>>,
    req: &OrderRequest,
    meta: &IngressMeta,
    ack: Option<&OrderAck>,
    status: u8,
) {
    let Some(tx) = result_tx else {
        return;
    };

    let wire = OrderResultWire {
        client_seq: req.client_seq,
        gateway_ts_ns: wall_clock_epoch_ns(),
        filled_size: 0,
        reject_code: 0,
        status,
        _pad0: [0; 3],
        exchange_order_id: ack
            .map(|a| encode_zero_padded::<48>(&a.exchange_order_id))
            .unwrap_or([0; 48]),
        text_tag: meta.text_tag,
        _reserved: [0; 16],
    };

    if let Err(TrySendError::Full(_)) = tx.try_send(wire) {
        warn!(
            target: "order_gateway::router",
            client_id = %req.client_id,
            "result channel full — dropping order result wire"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handle + start()
// ─────────────────────────────────────────────────────────────────────────────

/// 외부가 보유하는 task + cancel.
pub struct OrderGatewayHandle {
    /// router task.
    pub task: JoinHandle<()>,
    /// shutdown trigger.
    pub cancel: CancellationToken,
}

impl OrderGatewayHandle {
    /// 즉시 shutdown.
    pub fn shutdown(&self) {
        if !self.cancel.is_cancelled() {
            self.cancel.cancel();
        }
    }

    /// task join.
    pub async fn join(self) {
        if let Err(e) = self.task.await {
            warn!(
                target: "order_gateway::handle",
                error = ?e,
                "router task join error"
            );
        }
    }
}

/// 기본 start — dedup cap/retry 는 default.
pub fn start(
    routing: RoutingTable,
    rx: Receiver<IngressEnvelope>,
    ack_tx: Option<Sender<OrderAck>>,
    result_tx: Option<Sender<OrderResultWire>>,
) -> Result<OrderGatewayHandle> {
    start_with(
        routing,
        rx,
        ack_tx,
        result_tx,
        DEDUP_CACHE_CAP_DEFAULT,
        RetryPolicy::default(),
    )
}

/// 커스텀 dedup/retry 로 시작. `routing` 은 소유권 이동 — 내부에서 `Arc` 로 감싼다.
/// SHM subscriber 등 외부에서 같은 `RoutingTable` 를 공유해야 한다면
/// [`start_with_arc`] 를 사용.
pub fn start_with(
    routing: RoutingTable,
    rx: Receiver<IngressEnvelope>,
    ack_tx: Option<Sender<OrderAck>>,
    result_tx: Option<Sender<OrderResultWire>>,
    dedup_cap: usize,
    retry: RetryPolicy,
) -> Result<OrderGatewayHandle> {
    start_with_arc(Arc::new(routing), rx, ack_tx, result_tx, dedup_cap, retry)
}

/// [`start_with`] 와 동일하나 `Arc<RoutingTable>` 을 직접 받는다. 이 Arc 를 복제해
/// [`shm_sub::ShmOrderSubscriber::new`] 등에 넘기면 Cancel dispatch 를 공유할 수 있다.
///
/// 외부에서 routing 을 이미 `Arc` 로 들고 있는 경우 (SHM path + 내부 strategy 동시) 에
/// 복제 비용 없이 재사용할 수 있게 expose 한 얇은 래퍼.
pub fn start_with_arc(
    routing: Arc<RoutingTable>,
    rx: Receiver<IngressEnvelope>,
    ack_tx: Option<Sender<OrderAck>>,
    result_tx: Option<Sender<OrderResultWire>>,
    dedup_cap: usize,
    retry: RetryPolicy,
) -> Result<OrderGatewayHandle> {
    if routing.is_empty() {
        return Err(anyhow!("order gateway requires at least one route"));
    }

    let cancel = CancellationToken::new();
    let task_cancel = cancel.child_token();
    let dedup = Arc::new(DedupCache::new(dedup_cap));

    let router = Router::new(routing, rx, ack_tx, result_tx, dedup, retry);
    let task = tokio::spawn(async move { router.run(task_cancel).await });

    Ok(OrderGatewayHandle { task, cancel })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_exchange_api::{OrderSide, OrderType, TimeInForce};
    use hft_types::Symbol;

    fn sample_req(cid: &str) -> OrderRequest {
        OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 0.01,
            price: Some(100.0),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from(cid),
        }
    }

    fn sample_meta() -> IngressMeta {
        IngressMeta {
            origin_vm_id: Some(7),
            text_tag: encode_zero_padded::<32>("v8"),
        }
    }

    #[test]
    fn retry_backoff_grows_exponentially_and_clamps() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_ms: 100,
            cap_ms: 1000,
        };
        assert_eq!(p.backoff(1), Duration::from_millis(100));
        assert_eq!(p.backoff(2), Duration::from_millis(200));
        assert_eq!(p.backoff(3), Duration::from_millis(400));
        assert_eq!(p.backoff(4), Duration::from_millis(800));
        // clamp
        assert_eq!(p.backoff(5), Duration::from_millis(1000));
        assert_eq!(p.backoff(10), Duration::from_millis(1000));
    }

    #[test]
    fn dedup_cache_evicts_fifo() {
        let cache = DedupCache::new(2);
        let a: Arc<str> = Arc::from("a");
        let b: Arc<str> = Arc::from("b");
        let c: Arc<str> = Arc::from("c");

        assert!(!cache.check_and_insert(&a));
        assert!(!cache.check_and_insert(&b));
        // a, b 가 들어가 있음.
        assert!(cache.check_and_insert(&a));
        assert!(cache.check_and_insert(&b));

        // c 를 넣으면 a 가 evict.
        assert!(!cache.check_and_insert(&c));
        assert!(!cache.check_and_insert(&a), "a 는 evict 됐어야 한다");
    }

    #[test]
    fn routing_table_insert_get_len() {
        let mut t = RoutingTable::new();
        assert!(t.is_empty());
        t.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );
        assert_eq!(t.len(), 1);
        assert!(t.get(ExchangeId::Gate).is_some());
        assert!(t.get(ExchangeId::Binance).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_forwards_ack_through_noop_executor() {
        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );

        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (ack_tx, ack_rx) = crossbeam_channel::bounded::<OrderAck>(8);
        let handle = start(routing, req_rx, Some(ack_tx), None).unwrap();

        req_tx.send((sample_req("cid-1"), sample_meta())).unwrap();
        req_tx.send((sample_req("cid-2"), sample_meta())).unwrap();

        let ack1 = ack_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("ack1");
        let ack2 = ack_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("ack2");

        assert_eq!(ack1.exchange, ExchangeId::Gate);
        assert!(ack1.exchange_order_id.starts_with("noop-"));
        assert_eq!(ack2.client_id.as_ref(), "cid-2");

        handle.shutdown();
        handle.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_deduplicates_repeated_client_id() {
        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );

        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (ack_tx, ack_rx) = crossbeam_channel::bounded::<OrderAck>(8);
        let handle = start(routing, req_rx, Some(ack_tx), None).unwrap();

        req_tx.send((sample_req("same"), sample_meta())).unwrap();
        req_tx.send((sample_req("same"), sample_meta())).unwrap();
        req_tx.send((sample_req("same"), sample_meta())).unwrap();

        // 첫 번째만 ack.
        let ack = ack_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("first ack");
        assert_eq!(ack.client_id.as_ref(), "same");
        assert!(ack_rx.recv_timeout(Duration::from_millis(200)).is_err());

        handle.shutdown();
        handle.join().await;
    }

    /// 항상 실패하는 Executor — retry 테스트용.
    struct AlwaysFailExecutor {
        id: ExchangeId,
        calls: Arc<std::sync::atomic::AtomicU32>,
    }

    #[async_trait]
    impl ExchangeExecutor for AlwaysFailExecutor {
        fn id(&self) -> ExchangeId {
            self.id
        }
        async fn place_order(&self, _req: OrderRequest) -> Result<OrderAck> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(anyhow!("simulated network error"))
        }
        async fn cancel(&self, _id: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_retries_then_gives_up() {
        use std::sync::atomic::AtomicU32;
        let calls = Arc::new(AtomicU32::new(0));
        let exec = Arc::new(AlwaysFailExecutor {
            id: ExchangeId::Gate,
            calls: calls.clone(),
        });
        let mut routing = RoutingTable::new();
        routing.insert(ExchangeId::Gate, Route::Rust(exec));

        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (ack_tx, _ack_rx) = crossbeam_channel::bounded::<OrderAck>(8);

        // retry 를 짧게 재정의.
        let handle = start_with(
            routing,
            req_rx,
            Some(ack_tx),
            None,
            128,
            RetryPolicy {
                max_attempts: 2,
                base_ms: 1,
                cap_ms: 5,
            },
        )
        .unwrap();

        req_tx.send((sample_req("retry-cid"), sample_meta())).unwrap();

        // 2+1 = 3회 호출 후 포기.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 3
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);

        handle.shutdown();
        handle.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_skips_unknown_exchange() {
        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );
        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (ack_tx, ack_rx) = crossbeam_channel::bounded::<OrderAck>(8);
        let handle = start(routing, req_rx, Some(ack_tx), None).unwrap();

        // Binance 에는 route 가 없음 → drop.
        let mut req = sample_req("cid-bx");
        req.exchange = ExchangeId::Binance;
        req_tx.send((req, sample_meta())).unwrap();

        assert!(ack_rx.recv_timeout(Duration::from_millis(100)).is_err());
        handle.shutdown();
        handle.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_rejects_invalid_order() {
        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );
        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (ack_tx, ack_rx) = crossbeam_channel::bounded::<OrderAck>(8);
        let handle = start(routing, req_rx, Some(ack_tx), None).unwrap();

        // qty=0 → basic_validate 실패.
        let mut bad = sample_req("bad-1");
        bad.qty = 0.0;
        req_tx.send((bad, sample_meta())).unwrap();

        assert!(ack_rx.recv_timeout(Duration::from_millis(100)).is_err());
        handle.shutdown();
        handle.join().await;
    }

    #[test]
    fn start_rejects_empty_routing() {
        let routing = RoutingTable::new();
        let (_tx, rx) = crossbeam_channel::bounded::<IngressEnvelope>(1);
        let err = match start(routing, rx, None, None) {
            Ok(_) => panic!("start should reject empty routing"),
            Err(err) => err,
        };
        assert!(format!("{err}").contains("at least one route"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_emits_result_wire_with_text_tag_and_client_seq() {
        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );

        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<OrderResultWire>(8);
        let handle = start(routing, req_rx, None, Some(result_tx)).unwrap();

        req_tx
            .send((sample_req("cid-r1"), sample_meta()))
            .expect("send request");

        let wire = result_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("result wire");
        assert_eq!(wire.client_seq, 0);
        assert_eq!(wire.status, STATUS_ACCEPTED);
        assert_eq!(&wire.text_tag[..2], b"v8");

        handle.shutdown();
        handle.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_emits_rejected_result_wire_on_executor_error() {
        use std::sync::atomic::AtomicU32;

        let calls = Arc::new(AtomicU32::new(0));
        let exec = Arc::new(AlwaysFailExecutor {
            id: ExchangeId::Gate,
            calls,
        });
        let mut routing = RoutingTable::new();
        routing.insert(ExchangeId::Gate, Route::Rust(exec));

        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<OrderResultWire>(8);
        let handle = start_with(
            routing,
            req_rx,
            None,
            Some(result_tx),
            128,
            RetryPolicy {
                max_attempts: 0,
                base_ms: 1,
                cap_ms: 1,
            },
        )
        .unwrap();

        req_tx
            .send((sample_req("cid-rj"), sample_meta()))
            .expect("send request");

        let wire = result_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("rejected result wire");
        assert_eq!(wire.status, STATUS_REJECTED);
        assert_eq!(wire.client_seq, 0);
        assert_eq!(wire.exchange_order_id, [0; 48]);
        assert_eq!(&wire.text_tag[..2], b"v8");

        handle.shutdown();
        handle.join().await;
    }
}
