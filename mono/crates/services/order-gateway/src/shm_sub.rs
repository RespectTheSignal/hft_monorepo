//! ShmOrderSubscriber — Python strategy → Rust order-gateway SPSC ring consumer.
//!
//! Python (strategy VM) 이 order ring 에 `OrderFrame` 을 push 하면 gateway 쪽
//! 전용 OS thread 한 개가 즉시 consume 해 다음처럼 처리한다:
//!
//! - [`OrderKind::Place`] → `OrderFrame` 을 [`OrderRequest`] 로 디코드 후
//!   `req_tx` (Router 가 consume 하는 crossbeam 채널) 로 try_send. full 이면 drop +
//!   `OrderGatewayRouterQueueFull` counter.
//! - [`OrderKind::Cancel`] → `aux` 필드를 UTF-8 ASCII (null-padded) 로 디코드해
//!   `routing.get(...).cancel()` 를 tokio runtime 에서 `spawn` 으로 실행.
//!
//! ## v1 vs v2 경로
//! - **v1 legacy**: `/dev/shm/hft_orders_v2` 단일 ring 파일 + 단일 symtab 파일.
//!   [`ShmSubscriberSource::Legacy`].
//! - **v2 multi-VM**: [`SharedRegion`] 위의 N 개 `OrderRing` 을 round-robin 으로
//!   fan-in. gateway 가 [`MultiOrderRingReader`] 로 모든 ring 을 한 스레드에서
//!   polling. [`ShmSubscriberSource::SharedRegion`].
//!
//! # Hot path
//! `try_consume()` / `poll_batch()` 는 syscall 없음. Empty 시 adaptive backoff:
//! 1. `spin_loop` 최대 [`SPIN_LIMIT`] 회 — CPU 가 열려있을 때 최소 지연 추구.
//! 2. 그래도 없으면 `thread::park_timeout([`PARK_US`])` — CPU 양보.
//! 3. shutdown 시 `cancel.is_cancelled()` 체크로 즉시 break.
//!
//! # 실패 정책
//! - wire/format 손상(알 수 없는 kind/side/tif/ord_type 등) → `ShmOrderInvalid`
//! - 알 수 없는 exchange_id → `OrderGatewayExchangeNotFound`
//! - symtab.resolve miss → `OrderGatewayUnknownSymbol`
//! 파이프라인은 절대 정지시키지 않는다.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Sender, TrySendError};
use hft_exchange_api::{CancellationToken, OrderRequest, OrderSide, OrderType, TimeInForce};
use hft_shm::{
    exchange_from_u8, Backing, LayoutSpec, MultiOrderRingReader, OrderFrame, OrderKind,
    OrderRingReader, Role, SharedRegion, SubKind, SymbolTable,
};
use hft_telemetry::{counter_add, counter_inc, CounterKey};
use hft_types::Symbol;
use tokio::runtime::Handle as TokioHandle;
use tracing::{debug, error, info, warn};

use crate::{Route, RoutingTable};

/// 빈 ring 에서 CPU 양보 전 spin 시도 횟수. L2 레이턴시 ~10-30ns × 4096 ≈ 100μs.
const SPIN_LIMIT: u32 = 4096;
/// spin 소진 이후 park 시간. 너무 길면 주문 지연, 너무 짧으면 CPU burn.
const PARK_US: u64 = 50;
/// shutdown poll 주기 — park_timeout 이 깨어난 뒤에만 체크되므로 PARK_US 와 대략 동일.
const SHUTDOWN_POLL_MS: u64 = 5;
/// v2: poll_batch 의 한 바퀴당 ring 당 최대 소비 건수. 너무 크면 한 ring 이
/// 다른 ring 을 기아시킬 수 있어 작게 유지.
const MAX_PER_RING: usize = 32;

/// v2: publisher heartbeat poll 간격. 너무 짧으면 체크 오버헤드, 너무 길면 stale
/// 감지 지연. 50ms 는 `MULTI_VM_TOPOLOGY §6` 의 5초 alert 보다 100배 빠른 샘플링.
const HEARTBEAT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// v2: publisher heartbeat stale 판정 임계 (ns). 5s.
const HEARTBEAT_STALE_THRESHOLD_NS: u64 = 5 * 1_000_000_000;
// v2: heartbeat stale warn 중복 억제 — 같은 stale 구간에서 warn 을 1회로 제한.
// 복구되면 다시 arm. counter 는 매 체크마다 증가.
// (상수 자체는 없음 — 상태가 `HeartbeatWatcher` 내부 bool 로 관리됨.)
/// 현재 wall-clock ns. 테스트에서만 mock 하고, hot-path 는 SystemTime 한 번만 읽음.
fn now_realtime_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Publisher liveness 감시자. `Worker` 가 들고 있으며 주 루프에서 주기적으로 `tick`.
///
/// 동작:
/// - `HEARTBEAT_POLL_INTERVAL` (50ms) 마다만 실제 체크 수행 — 그 사이 `tick` 호출은
///   단순 `Instant::now()` 1회만 수행하고 즉시 복귀 (hot path 영향 미미).
/// - stale 감지 시 `CounterKey::ShmPublisherStale` 증가 + 최초 1회만 warn.
/// - 정상화 (age < threshold) 되면 warn arm 재설정 + info 로그 1회.
struct HeartbeatWatcher {
    shared: Arc<SharedRegion>,
    /// 마지막 체크 시각. 주기 필터링용.
    last_checked: Instant,
    /// 현재 stale 상태인가? — 같은 stale 구간에서 warn 을 한 번만 내보내기 위함.
    stale_latched: bool,
    /// publisher 가 touch 를 한 번이라도 했는가 — 초기 booting 구간 (age=None)
    /// 은 stale 로 간주하지 않음.
    ever_seen: bool,
}

impl HeartbeatWatcher {
    fn new(shared: Arc<SharedRegion>) -> Self {
        Self {
            shared,
            last_checked: Instant::now() - HEARTBEAT_POLL_INTERVAL,
            stale_latched: false,
            ever_seen: false,
        }
    }

    /// 주기 도달 시에만 실제 heartbeat 검사. hot path 에서 매 iteration 호출 OK.
    #[inline]
    fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_checked) < HEARTBEAT_POLL_INTERVAL {
            return;
        }
        self.last_checked = now;

        let hb = self.shared.heartbeat_ns();
        if hb != 0 {
            self.ever_seen = true;
        }
        if !self.ever_seen {
            return;
        }
        let wall_now = now_realtime_ns();
        let age = self.shared.heartbeat_age_ns(wall_now);
        match age {
            Some(age_ns) if age_ns > HEARTBEAT_STALE_THRESHOLD_NS => {
                counter_inc(CounterKey::ShmPublisherStale);
                if !self.stale_latched {
                    warn!(
                        target: "order_gateway::shm_sub",
                        age_ns = age_ns,
                        threshold_ns = HEARTBEAT_STALE_THRESHOLD_NS,
                        seq = self.shared.heartbeat_seq(),
                        "publisher heartbeat is stale — upstream may be stuck"
                    );
                    self.stale_latched = true;
                }
            }
            _ => {
                if self.stale_latched {
                    info!(
                        target: "order_gateway::shm_sub",
                        seq = self.shared.heartbeat_seq(),
                        "publisher heartbeat recovered"
                    );
                    self.stale_latched = false;
                }
            }
        }
    }
}

/// SHM order subscriber 핸들. `shutdown()` 은 idempotent.
pub struct ShmOrderSubscriberHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl ShmOrderSubscriberHandle {
    /// consumer thread 가 loop 을 빠져나오도록 요청. `join()` 은 별도 호출.
    /// `park_timeout` 중인 thread 를 즉시 깨워 shutdown 응답성을 높인다.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        if !self.cancel.is_cancelled() {
            self.cancel.cancel();
        }
        if let Some(t) = self.thread.as_ref() {
            t.thread().unpark();
        }
    }

    /// consumer thread join. 실패 시 warn 로그만 남기고 반환.
    pub fn join(mut self) {
        self.shutdown();
        if let Some(j) = self.thread.take() {
            if let Err(e) = j.join() {
                warn!(
                    target: "order_gateway::shm_sub",
                    error = ?e,
                    "shm order subscriber thread panicked on join"
                );
            }
        }
    }
}

impl Drop for ShmOrderSubscriberHandle {
    /// Drop 경로에서도 thread 가 영구 lingering 되지 않도록 stop flag 를 set.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// subscriber 가 읽어올 데이터 소스. legacy single-ring vs v2 multi-VM shared region.
#[derive(Debug, Clone)]
pub enum ShmSubscriberSource {
    /// v1 — `/dev/shm/hft_orders_v2` 단일 SPSC ring + 별도 symbol table 파일.
    Legacy {
        /// 단일 order ring path.
        order_path: PathBuf,
        /// 별도 symbol table path.
        symtab_path: PathBuf,
    },
    /// v2 — 단일 shared region 안의 N 개 order ring + 공용 symbol table.
    SharedRegion {
        /// 마운트할 파일 (또는 ivshmem PCI BAR) 경로.
        backing: Backing,
        /// region layout. publisher 와 byte-exact 일치해야 함.
        spec: LayoutSpec,
    },
}

/// 구성 파라미터. `hft_config::ShmConfig` 에서 채워 내보낸다.
#[derive(Debug, Clone)]
pub struct ShmSubscriberConfig {
    /// 데이터 소스 — legacy single-ring 또는 v2 shared region.
    pub source: ShmSubscriberSource,
}

impl ShmSubscriberConfig {
    /// 레거시 single-file 경로 설정을 만든다 (v1 호환).
    pub fn legacy(order_path: impl Into<PathBuf>, symtab_path: impl Into<PathBuf>) -> Self {
        Self {
            source: ShmSubscriberSource::Legacy {
                order_path: order_path.into(),
                symtab_path: symtab_path.into(),
            },
        }
    }

    /// v2 shared region 구성. `Role::OrderGateway` 로 attach.
    pub fn shared_region(backing: Backing, spec: LayoutSpec) -> Self {
        Self {
            source: ShmSubscriberSource::SharedRegion { backing, spec },
        }
    }
}

/// 내부 reader — legacy (단일) 또는 v2 (다중) 중 하나.
enum ReaderImpl {
    /// v1 단일 SPSC reader.
    Legacy(OrderRingReader),
    /// v2 N-way fan-in reader.
    MultiVm(MultiOrderRingReader),
}

/// OS thread 위에서 도는 실제 consumer.
/// `start()` 가 이 구조체를 생성해 즉시 thread 에 넘긴다 — 외부에 expose 하지 않음.
struct Worker {
    reader: ReaderImpl,
    symtab: SymbolTable,
    routing: Arc<RoutingTable>,
    req_tx: Sender<OrderRequest>,
    rt: TokioHandle,
    stop: Arc<AtomicBool>,
    cancel: CancellationToken,
    /// v2 SharedRegion 을 여기서 Arc 로 보관 — reader 들이 이 parent 를 참조한다.
    /// Legacy 경로에서는 None. Drop 순서 보장용 (reader 먼저 drop → region drop).
    _shared: Option<Arc<SharedRegion>>,
    /// v2: publisher heartbeat 감시자. Legacy 경로에서는 None.
    hb_watcher: Option<HeartbeatWatcher>,
    /// v2: `poll_once` 의 MultiVm 경로에서 할당 없이 재사용하는 scratch buffer.
    /// `Vec::clear()` 는 capacity 유지 → 첫 iteration 에서만 reserve, 이후 alloc 0.
    scratch_batch: Vec<(u32, OrderFrame)>,
}

/// SHM frame 처리 분류 오류.
enum HandleFrameError {
    /// wire/enum/포맷 자체가 손상됨.
    Invalid(anyhow::Error),
    /// exchange id 를 ExchangeId 로 해석할 수 없음.
    UnknownExchangeId(u8),
    /// symbol table 역조회 실패.
    UnknownSymbol(u32),
}

impl Worker {
    fn run(mut self) {
        let kind = match &self.reader {
            ReaderImpl::Legacy(_) => "legacy",
            ReaderImpl::MultiVm(m) => {
                info!(
                    target: "order_gateway::shm_sub",
                    n_rings = m.len(),
                    "shm order subscriber started (v2 multi-vm)"
                );
                "multi_vm"
            }
        };
        if kind == "legacy" {
            if let ReaderImpl::Legacy(ref r) = self.reader {
                info!(
                    target: "order_gateway::shm_sub",
                    reader_path = ?r.path(),
                    capacity = r.capacity(),
                    "shm order subscriber started (v1 legacy)"
                );
            }
        }

        let mut spins: u32 = 0;
        loop {
            if self.stop.load(Ordering::Acquire) || self.cancel.is_cancelled() {
                break;
            }

            let consumed = self.poll_once();
            // heartbeat 는 소비 여부와 무관하게 주기 도달 시 내부 필터로 체크.
            if let Some(hb) = self.hb_watcher.as_mut() {
                hb.tick();
            }
            if consumed > 0 {
                spins = 0;
                continue;
            }

            spins = spins.saturating_add(1);
            if spins < SPIN_LIMIT {
                std::hint::spin_loop();
            } else {
                counter_inc(CounterKey::ShmBackoffPark);
                thread::park_timeout(Duration::from_micros(PARK_US));
                if spins >= SPIN_LIMIT + 100_000 {
                    // 거의 idle — 더 길게 잠재. shutdown 반응성 5ms 허용.
                    thread::park_timeout(Duration::from_millis(SHUTDOWN_POLL_MS));
                }
            }
        }

        info!(target: "order_gateway::shm_sub", "shm order subscriber stopped");
    }

    /// 이번 iteration 에서 소비한 frame 수. 0 이면 backoff.
    fn poll_once(&mut self) -> usize {
        // legacy 경로는 single-frame 직행 — zero-alloc.
        if let ReaderImpl::Legacy(r) = &mut self.reader {
            return match r.try_consume() {
                Some(frame) => {
                    counter_inc(CounterKey::ShmOrderConsumed);
                    self.handle_one(0, &frame);
                    1
                }
                None => 0,
            };
        }
        // v2 경로: poll_batch closure 는 `&mut self` 를 빌릴 수 없어 (reader 가
        // 이미 &mut self 차지) 일단 batch 로 collect → borrow release 후 처리.
        // `scratch_batch` 는 Worker 수명 동안 단 한 번 reserve 되고 이후 clear()
        // 로만 리셋되므로 hot path alloc 은 0.
        let consumed = {
            // 두 필드를 동시에 &mut 로 — split borrow.
            let reader = &mut self.reader;
            let scratch = &mut self.scratch_batch;
            scratch.clear();
            match reader {
                ReaderImpl::MultiVm(m) => {
                    let needed = MAX_PER_RING * m.len();
                    if scratch.capacity() < needed {
                        scratch.reserve(needed - scratch.capacity());
                    }
                    m.poll_batch(MAX_PER_RING, |vm_id, frame| {
                        scratch.push((vm_id, frame));
                        true
                    })
                }
                _ => unreachable!(),
            }
        }; // 여기서 reader/scratch 의 &mut 가 drop 되어 아래에서 self 사용 가능.
        if consumed > 0 {
            counter_add(CounterKey::ShmOrderConsumed, consumed as u64);
            counter_inc(CounterKey::ShmOrderBatch);
            // clone 없이 iter 로 처리 (OrderFrame 은 Copy).
            // Borrowing self.scratch_batch 로 읽기만 하고, handle_one 은 &self.
            // rustc 가 분할 대여로 모호할 수 있어 index 루프로 단순화.
            for i in 0..self.scratch_batch.len() {
                let (vm_id, frame) = self.scratch_batch[i];
                self.handle_one(vm_id, &frame);
            }
        }
        consumed
    }

    /// frame 처리 wrapper — `handle_frame` 의 에러를 swallow 해 counter 로 집계.
    fn handle_one(&self, vm_id: u32, frame: &OrderFrame) {
        if let Err(e) = self.handle_frame(vm_id, frame) {
            match e {
                HandleFrameError::Invalid(err) => {
                    counter_inc(CounterKey::ShmOrderInvalid);
                    debug!(
                        target: "order_gateway::shm_sub",
                        vm_id,
                        seq = frame.seq,
                        kind = frame.kind,
                        error = %err,
                        "order frame decode failed — skipping"
                    );
                }
                HandleFrameError::UnknownExchangeId(exchange_id) => {
                    counter_inc(CounterKey::OrderGatewayExchangeNotFound);
                    debug!(
                        target: "order_gateway::shm_sub",
                        vm_id,
                        seq = frame.seq,
                        kind = frame.kind,
                        exchange_id,
                        "order frame exchange_id not found — skipping"
                    );
                }
                HandleFrameError::UnknownSymbol(symbol_idx) => {
                    counter_inc(CounterKey::OrderGatewayUnknownSymbol);
                    debug!(
                        target: "order_gateway::shm_sub",
                        vm_id,
                        seq = frame.seq,
                        kind = frame.kind,
                        symbol_idx,
                        "order frame symbol_idx not found — skipping"
                    );
                }
            }
        }
    }

    /// 한 frame 을 decode 후 적절한 경로로 dispatch. 실패 시 `Err` 로 counter 증가.
    fn handle_frame(&self, vm_id: u32, frame: &OrderFrame) -> Result<(), HandleFrameError> {
        // 1) exchange id.
        let exchange = exchange_from_u8(frame.exchange_id)
            .ok_or(HandleFrameError::UnknownExchangeId(frame.exchange_id))?;

        // 2) symbol resolve via symbol table.
        let (ex_resolved, symbol_name) = self
            .symtab
            .resolve(frame.symbol_idx)
            .ok_or(HandleFrameError::UnknownSymbol(frame.symbol_idx))?;
        if ex_resolved != exchange {
            return Err(HandleFrameError::Invalid(anyhow!(
                "symbol_idx={} belongs to {:?}, not {:?}",
                frame.symbol_idx,
                ex_resolved,
                exchange
            )));
        }
        let symbol = Symbol::new(&symbol_name);

        // 3) kind dispatch.
        let kind = match frame.kind {
            x if x == OrderKind::Place as u8 => OrderKind::Place,
            x if x == OrderKind::Cancel as u8 => OrderKind::Cancel,
            x => return Err(HandleFrameError::Invalid(anyhow!("unknown OrderKind={}", x))),
        };

        match kind {
            OrderKind::Place => self.dispatch_place(vm_id, frame, exchange, symbol),
            OrderKind::Cancel => self.dispatch_cancel(vm_id, frame, exchange),
        }
    }

    fn dispatch_place(
        &self,
        vm_id: u32,
        frame: &OrderFrame,
        exchange: hft_types::ExchangeId,
        symbol: Symbol,
    ) -> Result<(), HandleFrameError> {
        let side = match frame.side {
            0 => OrderSide::Buy,
            1 => OrderSide::Sell,
            x => return Err(HandleFrameError::Invalid(anyhow!("unknown side={}", x))),
        };
        let tif = match frame.tif {
            0 => TimeInForce::Gtc,
            1 => TimeInForce::Ioc,
            2 => TimeInForce::Fok,
            x => return Err(HandleFrameError::Invalid(anyhow!("unknown tif={}", x))),
        };
        let order_type = match frame.ord_type {
            0 => OrderType::Limit,
            1 => OrderType::Market,
            x => return Err(HandleFrameError::Invalid(anyhow!("unknown ord_type={}", x))),
        };

        // SHM 는 i64-scaled 표현이지만 legacy publisher 와 동일하게 `as i64 / as f64`
        // 직접 cast 규약을 따른다. (Phase 2 이후 고정밀 Decimal 도입 가능.)
        let qty = frame.size as f64;
        if !qty.is_finite() || qty <= 0.0 {
            return Err(HandleFrameError::Invalid(anyhow!(
                "non-positive size={}",
                frame.size
            )));
        }
        let price = match order_type {
            OrderType::Limit => {
                let p = frame.price as f64;
                if !p.is_finite() || p <= 0.0 {
                    return Err(HandleFrameError::Invalid(anyhow!(
                        "non-positive limit price={}",
                        frame.price
                    )));
                }
                Some(p)
            }
            OrderType::Market => None,
        };

        // client_id: v2 에서는 strategy VM 간 충돌을 막기 위해 vm_id prefix 부착.
        // legacy (vm_id=0) 에서도 prefix 를 쓰지만, 기존 테스트 호환을 위해
        // vm_id=0 일 때의 형식은 "py-{exchange}-{client_id}" 로 유지.
        let cid_str = if vm_id == 0 {
            format!("py-{}-{}", exchange.as_str(), frame.client_id)
        } else {
            format!("py{}-{}-{}", vm_id, exchange.as_str(), frame.client_id)
        };
        let client_id: Arc<str> = Arc::from(cid_str);

        let req = OrderRequest {
            exchange,
            symbol,
            side,
            order_type,
            qty,
            price,
            // TODO(Step5-debt): OrderFrame 에 reduce_only 부재. SHM path 는 현재 false 고정.
            reduce_only: false,
            tif,
            client_seq: frame.client_id,
            origin_ts_ns: frame.ts_ns,
            client_id,
        };

        // Router 로 전달 (핫패스 아님: crossbeam try_send).
        match self.req_tx.try_send(req) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(req)) => {
                counter_inc(CounterKey::OrderGatewayRouterQueueFull);
                warn!(
                    target: "order_gateway::shm_sub",
                    vm_id,
                    client_id = %req.client_id,
                    exchange = ?req.exchange,
                    "order router channel full — dropping SHM-ingested order"
                );
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => {
                error!(
                    target: "order_gateway::shm_sub",
                    "order router channel disconnected — stopping SHM subscriber"
                );
                self.stop.store(true, Ordering::Release);
                Ok(())
            }
        }
    }

    fn dispatch_cancel(
        &self,
        vm_id: u32,
        frame: &OrderFrame,
        exchange: hft_types::ExchangeId,
    ) -> Result<(), HandleFrameError> {
        // aux (5 × u64 = 40B) 를 little-endian bytes 로 읽어 null-trimmed ASCII 로 해석.
        let mut raw = [0u8; 40];
        for (i, w) in frame.aux.iter().enumerate() {
            raw[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        let trimmed = raw.split(|b| *b == 0).next().unwrap_or(&[]);
        let exchange_order_id = std::str::from_utf8(trimmed)
            .map_err(|e| HandleFrameError::Invalid(anyhow!("aux is not ASCII: {e}")))?
            .trim();
        if exchange_order_id.is_empty() {
            return Err(HandleFrameError::Invalid(anyhow!(
                "empty exchange_order_id in Cancel frame"
            )));
        }

        let route = self
            .routing
            .get(exchange)
            .ok_or(HandleFrameError::UnknownExchangeId(frame.exchange_id))?;

        let exec = match route {
            Route::Rust(e) => e.clone(),
            Route::GoIpc { endpoint } => {
                return Err(HandleFrameError::Invalid(anyhow!(
                    "cancel via Go IPC (endpoint={endpoint}) not implemented"
                )));
            }
        };

        let id_string = exchange_order_id.to_string();
        // tokio spawn — cancel 은 ms 허용, blocking 하지 않는다.
        self.rt.spawn(async move {
            counter_inc(CounterKey::ShmOrderCancelDispatched);
            if let Err(e) = exec.cancel(&id_string).await {
                warn!(
                    target: "order_gateway::shm_sub",
                    vm_id,
                    exchange_order_id = %id_string,
                    error = %e,
                    "cancel via SHM path failed"
                );
            }
        });
        Ok(())
    }
}

/// SHM subscriber 기동. 실패 시 caller 는 Noop/ZMQ 경로만 유지.
///
/// # Arguments
/// - `cfg`: legacy 경로 또는 v2 shared region 구성.
/// - `routing`: `start_with_arc` 와 동일 인스턴스를 공유해야 Cancel dispatch 가 가능.
/// - `req_tx`: Router 가 consume 하는 `OrderRequest` 채널.
/// - `rt`: 현재 tokio runtime 의 `Handle`.
/// - `cancel`: 상위 cancellation token.
pub fn start_shm_order_subscriber(
    cfg: &ShmSubscriberConfig,
    routing: Arc<RoutingTable>,
    req_tx: Sender<OrderRequest>,
    rt: TokioHandle,
    cancel: CancellationToken,
) -> Result<ShmOrderSubscriberHandle> {
    let (reader, symtab, shared) = match &cfg.source {
        ShmSubscriberSource::Legacy {
            order_path,
            symtab_path,
        } => {
            let reader = open_reader(order_path)
                .with_context(|| format!("open order ring at {order_path:?}"))?;
            let symtab = open_symtab(symtab_path)
                .with_context(|| format!("open symbol table at {symtab_path:?}"))?;
            (ReaderImpl::Legacy(reader), symtab, None)
        }
        ShmSubscriberSource::SharedRegion { backing, spec } => {
            let sr = SharedRegion::create_or_attach(
                backing.clone(),
                *spec,
                Role::OrderGateway,
            )
            .with_context(|| format!("attach SharedRegion at {backing:?}"))?;
            // order rings 은 strict attach — publisher 가 먼저 init 완료 전이라면
            // 여기서 실패하고 caller 가 재시도해야 함. 운영상 publisher 가 먼저
            // 뜨는 규약이므로 strict 로 둔다.
            let m = MultiOrderRingReader::attach(&sr)
                .context("attach MultiOrderRingReader")?;
            // symbol table 은 read/write 모두 가능한 모드로 연다 (writer 일 필요 없음).
            let sub = sr
                .sub_region(SubKind::Symtab)
                .context("sub_region(Symtab)")?;
            let symtab = SymbolTable::open_from_region(sub)
                .map_err(|e| anyhow!("open symtab from region: {e}"))?;
            (ReaderImpl::MultiVm(m), symtab, Some(Arc::new(sr)))
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    let cancel_c = cancel.clone();

    // v2 경로면 heartbeat watcher 를 붙인다. Legacy 는 watcher 없음.
    let hb_watcher = shared.as_ref().map(|sr| HeartbeatWatcher::new(sr.clone()));
    // scratch buffer 는 v2 전용 — 실제 capacity reserve 는 poll_once 첫 iteration 에서.
    let scratch_batch = Vec::new();

    let worker = Worker {
        reader,
        symtab,
        routing,
        req_tx,
        rt,
        stop: stop_c,
        cancel: cancel_c,
        _shared: shared,
        hb_watcher,
        scratch_batch,
    };

    let thread = thread::Builder::new()
        .name("shm-order-sub".into())
        .spawn(move || worker.run())
        .context("spawn shm-order-sub thread")?;

    Ok(ShmOrderSubscriberHandle {
        stop,
        thread: Some(thread),
        cancel,
    })
}

/// `OrderRingReader::open` wrapper — Send-transparent, `anyhow` 변환.
fn open_reader(path: &Path) -> Result<OrderRingReader> {
    OrderRingReader::open(path).map_err(|e| anyhow!("{e}"))
}

/// `SymbolTable::open` wrapper.
fn open_symtab(path: &Path) -> Result<SymbolTable> {
    SymbolTable::open(path).map_err(|e| anyhow!("{e}"))
}

/// Python-producer 측 디버깅/테스트용 헬퍼. Cancel frame 용 aux encoding.
pub fn encode_exchange_order_id(exchange_order_id: &str) -> Result<[u64; 5]> {
    let b = exchange_order_id.as_bytes();
    if b.len() > 40 {
        return Err(anyhow!("exchange_order_id too long ({} > 40)", b.len()));
    }
    let mut buf = [0u8; 40];
    buf[..b.len()].copy_from_slice(b);
    let mut out = [0u64; 5];
    for i in 0..5 {
        out[i] = u64::from_le_bytes(<[u8; 8]>::try_from(&buf[i * 8..i * 8 + 8]).unwrap());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hft_exchange_api::{ExchangeExecutor, OrderAck};
    use hft_shm::OrderRingWriter;
    use hft_telemetry::counters_snapshot;
    use hft_types::ExchangeId;
    use std::sync::atomic::AtomicU32;
    use tempfile::tempdir;

    fn counter_value(key: &str) -> u64 {
        counters_snapshot()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or(0)
    }

    fn sample_place_frame(symbol_idx: u32, client_id: u64) -> OrderFrame {
        OrderFrame {
            seq: 0,
            kind: OrderKind::Place as u8,
            exchange_id: hft_shm::exchange_to_u8(ExchangeId::Gate),
            _pad1: [0; 2],
            symbol_idx,
            side: 0, // Buy
            tif: 0,  // GTC
            ord_type: 0, // Limit
            _pad2: [0; 1],
            price: 50_000,
            size: 1,
            client_id,
            ts_ns: 12345,
            aux: [0; 5],
            _pad3: [0; 16],
        }
    }

    fn sample_cancel_frame(symbol_idx: u32, order_id: &str) -> OrderFrame {
        OrderFrame {
            seq: 0,
            kind: OrderKind::Cancel as u8,
            exchange_id: hft_shm::exchange_to_u8(ExchangeId::Gate),
            _pad1: [0; 2],
            symbol_idx,
            side: 0,
            tif: 0,
            ord_type: 0,
            _pad2: [0; 1],
            price: 0,
            size: 0,
            client_id: 0,
            ts_ns: 0,
            aux: encode_exchange_order_id(order_id).unwrap(),
            _pad3: [0; 16],
        }
    }

    /// Cancel 호출 횟수를 집계하는 Executor.
    struct CountingExecutor {
        id: ExchangeId,
        cancels: Arc<AtomicU32>,
        last_id: Arc<parking_lot::Mutex<String>>,
    }

    #[async_trait]
    impl ExchangeExecutor for CountingExecutor {
        fn id(&self) -> ExchangeId {
            self.id
        }
        async fn place_order(&self, req: OrderRequest) -> Result<OrderAck> {
            Ok(OrderAck {
                exchange: self.id,
                exchange_order_id: format!("counting-{}", req.client_id),
                client_id: req.client_id,
                ts_ms: 0,
            })
        }
        async fn cancel(&self, exchange_order_id: &str) -> Result<()> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            *self.last_id.lock() = exchange_order_id.to_string();
            Ok(())
        }
    }

    fn setup_shm(dir: &std::path::Path) -> (OrderRingWriter, SymbolTable, u32) {
        let op = dir.join("orders");
        let sp = dir.join("symtab");
        let writer = OrderRingWriter::create(&op, 64).unwrap();
        let sym = SymbolTable::open_or_create(&sp, 32).unwrap();
        let idx = sym
            .get_or_intern(ExchangeId::Gate, "BTC_USDT")
            .unwrap();
        (writer, sym, idx)
    }

    #[tokio::test]
    async fn place_frame_forwards_as_order_request() {
        let dir = tempdir().unwrap();
        let (writer, _sym, sym_idx) = setup_shm(dir.path());

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(crate::NoopExecutor::new(ExchangeId::Gate))),
        );
        let routing = Arc::new(routing);

        let (req_tx, req_rx) = crossbeam_channel::bounded::<OrderRequest>(16);

        let cancel = CancellationToken::new();
        let cfg = ShmSubscriberConfig::legacy(
            dir.path().join("orders"),
            dir.path().join("symtab"),
        );
        let handle = start_shm_order_subscriber(
            &cfg,
            routing.clone(),
            req_tx,
            TokioHandle::current(),
            cancel.clone(),
        )
        .unwrap();

        // Python-producer 역할.
        assert!(writer.publish(&sample_place_frame(sym_idx, 42)));

        let req = tokio::task::spawn_blocking(move || {
            req_rx.recv_timeout(Duration::from_millis(500))
        })
        .await
        .unwrap()
        .expect("router should receive OrderRequest");

        assert_eq!(req.exchange, ExchangeId::Gate);
        assert_eq!(req.symbol.as_str(), "BTC_USDT");
        assert_eq!(req.side, OrderSide::Buy);
        assert_eq!(req.order_type, OrderType::Limit);
        assert!((req.qty - 1.0).abs() < 1e-9);
        assert_eq!(req.price, Some(50_000.0));
        assert!(req.client_id.starts_with("py-gate-"));

        handle.join();
    }

    #[tokio::test]
    async fn cancel_frame_dispatches_to_executor() {
        let dir = tempdir().unwrap();
        let (writer, _sym, sym_idx) = setup_shm(dir.path());

        let cancels = Arc::new(AtomicU32::new(0));
        let last_id = Arc::new(parking_lot::Mutex::new(String::new()));
        let exec = Arc::new(CountingExecutor {
            id: ExchangeId::Gate,
            cancels: cancels.clone(),
            last_id: last_id.clone(),
        });
        let mut routing = RoutingTable::new();
        routing.insert(ExchangeId::Gate, Route::Rust(exec));
        let routing = Arc::new(routing);

        let (req_tx, _req_rx) = crossbeam_channel::bounded::<OrderRequest>(16);
        let cancel = CancellationToken::new();
        let cfg = ShmSubscriberConfig::legacy(
            dir.path().join("orders"),
            dir.path().join("symtab"),
        );
        let handle = start_shm_order_subscriber(
            &cfg,
            routing.clone(),
            req_tx,
            TokioHandle::current(),
            cancel.clone(),
        )
        .unwrap();

        // publish cancel frame.
        assert!(writer.publish(&sample_cancel_frame(sym_idx, "ORD-42-XYZ")));

        // wait for cancel to register.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while cancels.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(cancels.load(Ordering::SeqCst), 1);
        assert_eq!(last_id.lock().as_str(), "ORD-42-XYZ");

        handle.join();
    }

    #[tokio::test]
    async fn invalid_exchange_id_is_skipped() {
        let dir = tempdir().unwrap();
        let (writer, _sym, _idx) = setup_shm(dir.path());

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(crate::NoopExecutor::new(ExchangeId::Gate))),
        );
        let routing = Arc::new(routing);

        let (req_tx, req_rx) = crossbeam_channel::bounded::<OrderRequest>(16);
        let cancel = CancellationToken::new();
        let cfg = ShmSubscriberConfig::legacy(
            dir.path().join("orders"),
            dir.path().join("symtab"),
        );
        let handle = start_shm_order_subscriber(
            &cfg,
            routing.clone(),
            req_tx,
            TokioHandle::current(),
            cancel.clone(),
        )
        .unwrap();

        // bad exchange id.
        let mut bad = sample_place_frame(0, 7);
        bad.exchange_id = 99;
        assert!(writer.publish(&bad));

        // shouldn't receive anything.
        let got = tokio::task::spawn_blocking(move || {
            req_rx.recv_timeout(Duration::from_millis(100))
        })
        .await
        .unwrap();
        assert!(got.is_err());

        handle.join();
    }

    #[tokio::test]
    async fn v2_multi_vm_fan_in() {
        use hft_shm::{LayoutSpec, OrderRingWriter};

        let dir = tempdir().unwrap();
        let p = dir.path().join("v2-shared");
        let spec = LayoutSpec {
            quote_slot_count: 8,
            trade_ring_capacity: 16,
            symtab_capacity: 32,
            order_ring_capacity: 16,
            n_max: 3,
        };

        // publisher 가 먼저 region 을 만들고 symtab/order-ring header 를 초기화.
        let sr_pub = SharedRegion::create_or_attach(
            Backing::DevShm { path: p.clone() },
            spec,
            Role::Publisher,
        )
        .unwrap();
        let sym_w = SymbolTable::from_region(
            sr_pub.sub_region(SubKind::Symtab).unwrap(),
            spec.symtab_capacity,
        )
        .unwrap();
        let sym_idx = sym_w.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();

        // VM 2개의 writer open (vm_id 1, 2).
        let w1 = OrderRingWriter::from_region(
            sr_pub.sub_region(SubKind::OrderRing { vm_id: 1 }).unwrap(),
            spec.order_ring_capacity,
        )
        .unwrap();
        let w2 = OrderRingWriter::from_region(
            sr_pub.sub_region(SubKind::OrderRing { vm_id: 2 }).unwrap(),
            spec.order_ring_capacity,
        )
        .unwrap();
        // VM 0 (미사용) 도 publisher 가 init 해줘야 gateway strict attach 가 통과.
        let _w0 = OrderRingWriter::from_region(
            sr_pub.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap(),
            spec.order_ring_capacity,
        )
        .unwrap();

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(crate::NoopExecutor::new(ExchangeId::Gate))),
        );
        let routing = Arc::new(routing);

        let (req_tx, req_rx) = crossbeam_channel::bounded::<OrderRequest>(32);
        let cancel = CancellationToken::new();

        let cfg = ShmSubscriberConfig::shared_region(
            Backing::DevShm { path: p.clone() },
            spec,
        );
        let handle = start_shm_order_subscriber(
            &cfg,
            routing.clone(),
            req_tx,
            TokioHandle::current(),
            cancel.clone(),
        )
        .unwrap();

        // 각 VM 에서 2건씩 push.
        for i in 0..2u64 {
            assert!(w1.publish(&sample_place_frame(sym_idx, 100 + i)));
            assert!(w2.publish(&sample_place_frame(sym_idx, 200 + i)));
        }

        let mut collected: Vec<OrderRequest> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        while collected.len() < 4 && std::time::Instant::now() < deadline {
            if let Ok(r) = req_rx.recv_timeout(Duration::from_millis(50)) {
                collected.push(r);
            }
        }
        assert_eq!(collected.len(), 4, "got {} requests", collected.len());
        // VM 별 prefix 확인. py1-gate-100, py1-gate-101, py2-gate-200, py2-gate-201.
        let mut vm1 = 0;
        let mut vm2 = 0;
        for r in &collected {
            if r.client_id.starts_with("py1-") {
                vm1 += 1;
            } else if r.client_id.starts_with("py2-") {
                vm2 += 1;
            }
        }
        assert_eq!(vm1, 2);
        assert_eq!(vm2, 2);

        handle.join();
    }

    #[tokio::test]
    async fn request_channel_full_counts_router_queue_full() {
        let dir = tempdir().unwrap();
        let (writer, _sym, sym_idx) = setup_shm(dir.path());

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(crate::NoopExecutor::new(ExchangeId::Gate))),
        );
        let routing = Arc::new(routing);

        let (req_tx, _req_rx) = crossbeam_channel::bounded::<OrderRequest>(1);
        let cancel = CancellationToken::new();
        let cfg = ShmSubscriberConfig::legacy(
            dir.path().join("orders"),
            dir.path().join("symtab"),
        );
        let handle = start_shm_order_subscriber(
            &cfg,
            routing,
            req_tx,
            TokioHandle::current(),
            cancel.clone(),
        )
        .unwrap();

        let before = counter_value("order_gateway_router_queue_full");
        assert!(writer.publish(&sample_place_frame(sym_idx, 1)));
        assert!(writer.publish(&sample_place_frame(sym_idx, 2)));

        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while counter_value("order_gateway_router_queue_full") <= before
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(counter_value("order_gateway_router_queue_full") > before);

        handle.join();
    }

    #[tokio::test]
    async fn multi_vm_consumed_counter_is_per_frame() {
        use hft_shm::{LayoutSpec, OrderRingWriter};

        let dir = tempdir().unwrap();
        let p = dir.path().join("v2-shared-counter");
        let spec = LayoutSpec {
            quote_slot_count: 8,
            trade_ring_capacity: 16,
            symtab_capacity: 32,
            order_ring_capacity: 16,
            n_max: 3,
        };

        let sr_pub = SharedRegion::create_or_attach(
            Backing::DevShm { path: p.clone() },
            spec,
            Role::Publisher,
        )
        .unwrap();
        let sym_w = SymbolTable::from_region(
            sr_pub.sub_region(SubKind::Symtab).unwrap(),
            spec.symtab_capacity,
        )
        .unwrap();
        let sym_idx = sym_w.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();

        let w1 = OrderRingWriter::from_region(
            sr_pub.sub_region(SubKind::OrderRing { vm_id: 1 }).unwrap(),
            spec.order_ring_capacity,
        )
        .unwrap();
        let w2 = OrderRingWriter::from_region(
            sr_pub.sub_region(SubKind::OrderRing { vm_id: 2 }).unwrap(),
            spec.order_ring_capacity,
        )
        .unwrap();
        let _w0 = OrderRingWriter::from_region(
            sr_pub.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap(),
            spec.order_ring_capacity,
        )
        .unwrap();

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(crate::NoopExecutor::new(ExchangeId::Gate))),
        );
        let routing = Arc::new(routing);

        let (req_tx, req_rx) = crossbeam_channel::bounded::<OrderRequest>(32);
        let cancel = CancellationToken::new();
        let cfg = ShmSubscriberConfig::shared_region(
            Backing::DevShm { path: p.clone() },
            spec,
        );
        let handle = start_shm_order_subscriber(
            &cfg,
            routing,
            req_tx,
            TokioHandle::current(),
            cancel.clone(),
        )
        .unwrap();

        let before = counter_value("shm_order_consumed");
        for i in 0..2u64 {
            assert!(w1.publish(&sample_place_frame(sym_idx, 10 + i)));
            assert!(w2.publish(&sample_place_frame(sym_idx, 20 + i)));
        }

        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        let mut received = 0usize;
        while received < 4 && std::time::Instant::now() < deadline {
            if req_rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                received += 1;
            }
        }
        assert_eq!(received, 4);

        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while counter_value("shm_order_consumed") < before + 4
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(counter_value("shm_order_consumed") >= before + 4);

        handle.join();
    }

    #[test]
    fn encode_exchange_order_id_roundtrip() {
        let s = "ORD-ABC-123456789";
        let aux = encode_exchange_order_id(s).unwrap();
        // reverse the encoding the same way Worker::dispatch_cancel does.
        let mut raw = [0u8; 40];
        for (i, w) in aux.iter().enumerate() {
            raw[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        let trimmed = raw.split(|b| *b == 0).next().unwrap();
        assert_eq!(std::str::from_utf8(trimmed).unwrap(), s);
    }

    #[test]
    fn encode_exchange_order_id_rejects_too_long() {
        let s = "X".repeat(41);
        assert!(encode_exchange_order_id(&s).is_err());
    }
}
