//! hft-strategy-shm — Strategy VM 측 v2 SharedRegion facade.
//!
//! Multi-VM 토폴로지의 **세 번째 꼭짓점** (Publisher / OrderGateway / **Strategy**)
//! 에 해당하는 얇은 wrapper 크레이트. SharedRegion 위에서 전략 VM 이 필요로 하는
//! 세 가지 핸들 — quote reader, trade reader, 자기 vm_id 의 order writer — 를
//! 한 번에 열어 묶어 준다.
//!
//! ## 동기
//!
//! 이전에는 각 strategy 구현체가 다음 보일러를 직접 들고 있었다:
//!
//! ```ignore
//! let sr = SharedRegion::open_view(backing, spec, Role::Strategy { vm_id })?;
//! let q = QuoteSlotReader::from_region(sr.sub_region(SubKind::Quote)?)?;
//! let t = TradeRingReader::from_region(sr.sub_region(SubKind::Trade)?)?;
//! let s = SymbolTable::open_from_region(sr.sub_region(SubKind::Symtab)?)?;
//! let ow = OrderRingWriter::from_region(
//!     sr.sub_region(SubKind::OrderRing { vm_id })?, spec.order_ring_capacity)?;
//! ```
//!
//! `hft-strategy-shm` 은 이 5줄을 [`StrategyShmClient::attach`] 한 줄로 축약한다.
//!
//! ## Hot path
//!
//! - `publish_order(symbol, frame_without_idx)` — symbol intern 1회 (DashMap 히트)
//!   + SPSC push 1회 = ~30ns.
//! - `poll_quote(idx)` / `poll_trade()` — readers 바로 사용.
//!
//! ## 실패 정책
//!
//! [`StrategyShmClient::attach`] 는 publisher 가 SharedRegion 을 이미 초기화했다는
//! 전제로 **strict**. magic/digest 검증 실패 시 에러. caller 는 필요 시 재시도 루프
//! (`SHM_BOOT_RETRY`) 로 감싸면 됨.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hft_shm::{
    Backing, LayoutSpec, OrderFrame, OrderRingWriter, QuoteSlotReader, QuoteSnapshot, Role,
    SharedRegion, SubKind, SymbolTable, TradeFrame, TradeRingReader,
};
use hft_types::ExchangeId;
use tracing::{debug, info, warn};

/// Strategy VM 이 단일 SharedRegion 에 연결해 얻는 통합 핸들.
///
/// 구조:
/// - `quote`: [`QuoteSlotReader`] — publisher 가 덮어쓰는 bid/ask 최신값 읽기.
/// - `trade`: [`TradeRingReader`] — SPMC broadcast, 자기 cursor 만 전진.
/// - `symtab`: [`SymbolTable`] — `(exchange, symbol)` ↔ `symbol_idx`.
/// - `order`: [`OrderRingWriter`] — **자기 `vm_id`** 의 SPSC 주문 ring.
/// - `shared`: 원본 [`SharedRegion`] — heartbeat 조회 / sub_view 추가용.
///
/// # 불변식
/// 1. `attach` 는 `Role::Strategy { vm_id }` 로 region 을 연다.
/// 2. OrderRing 은 오로지 `vm_id` 번째만 — 다른 ring 에는 쓰지 않는다
///    (gateway 쪽 "ring 당 1 producer" 규약 보호).
pub struct StrategyShmClient {
    shared: SharedRegion,
    quote: QuoteSlotReader,
    trade: TradeRingReader,
    symtab: SymbolTable,
    order: OrderRingWriter,
    vm_id: u32,
}

impl std::fmt::Debug for StrategyShmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrategyShmClient")
            .field("vm_id", &self.vm_id)
            .field("role", &self.shared.role())
            .field("n_max", &self.shared.spec().n_max)
            .finish()
    }
}

impl StrategyShmClient {
    /// SharedRegion 에 strict 으로 attach 하고 3종 핸들을 구성한다.
    ///
    /// - `backing`: publisher 가 사용한 것과 동일 백엔드 (dev_shm / hugetlbfs /
    ///   PciBar). 전략 VM 이 ivshmem guest 라면 보통 `Backing::PciBar`.
    /// - `spec`: publisher 와 **byte-exact** 동일 spec. 다르면 digest mismatch.
    /// - `vm_id`: `0..spec.n_max`. 각 전략 VM 이 자기 고유 값을 부트 시 전달.
    ///
    /// # 실패
    /// - digest mismatch: `ShmError::Other("layout digest mismatch...")`.
    /// - `vm_id >= n_max`: 정책 위반.
    /// - Publisher 가 아직 해당 ring 의 magic 을 쓰지 않았으면 `from_region` 실패.
    pub fn attach(backing: Backing, spec: LayoutSpec, vm_id: u32) -> Result<Self> {
        if vm_id >= spec.n_max {
            return Err(anyhow!(
                "vm_id {} out of range (n_max={})",
                vm_id,
                spec.n_max
            ));
        }
        let shared = SharedRegion::open_view(backing.clone(), spec, Role::Strategy { vm_id })
            .with_context(|| format!("open_view SharedRegion at {backing:?}"))?;

        let quote = QuoteSlotReader::from_region(
            shared
                .sub_region(SubKind::Quote)
                .context("sub_region(Quote)")?,
        )
        .map_err(|e| anyhow!("QuoteSlotReader::from_region: {e}"))?;

        let trade = TradeRingReader::from_region(
            shared
                .sub_region(SubKind::Trade)
                .context("sub_region(Trade)")?,
        )
        .map_err(|e| anyhow!("TradeRingReader::from_region: {e}"))?;

        let symtab = SymbolTable::open_from_region(
            shared
                .sub_region(SubKind::Symtab)
                .context("sub_region(Symtab)")?,
        )
        .map_err(|e| anyhow!("SymbolTable::open_from_region: {e}"))?;

        let order = OrderRingWriter::from_region(
            shared
                .sub_region(SubKind::OrderRing { vm_id })
                .context("sub_region(OrderRing)")?,
            spec.order_ring_capacity,
        )
        .map_err(|e| anyhow!("OrderRingWriter::from_region: {e}"))?;

        info!(
            target: "strategy::shm",
            vm_id,
            n_max = spec.n_max,
            "StrategyShmClient attached"
        );

        Ok(Self {
            shared,
            quote,
            trade,
            symtab,
            order,
            vm_id,
        })
    }

    /// Boot 타이밍 레이스에 대비한 재시도 attach. publisher 가 뜨기 전에 strategy
    /// 가 먼저 구동되는 운영 상황을 허용한다. `timeout` 동안 `step` 간격으로
    /// `attach` 를 반복.
    pub fn attach_with_retry(
        backing: Backing,
        spec: LayoutSpec,
        vm_id: u32,
        timeout: Duration,
        step: Duration,
    ) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            match Self::attach(backing.clone(), spec, vm_id) {
                Ok(c) => return Ok(c),
                Err(e) => {
                    debug!(
                        target: "strategy::shm",
                        vm_id,
                        error = %e,
                        "attach retry"
                    );
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                }
            }
            std::thread::sleep(step);
        }
    }

    /// Quote slot reader.
    pub fn quote_reader(&self) -> &QuoteSlotReader {
        &self.quote
    }
    /// Trade ring reader (mut — cursor 전진).
    pub fn trade_reader(&mut self) -> &mut TradeRingReader {
        &mut self.trade
    }
    /// Symbol table.
    pub fn symtab(&self) -> &SymbolTable {
        &self.symtab
    }
    /// 자기 vm_id 의 order ring writer.
    pub fn order_writer(&self) -> &OrderRingWriter {
        &self.order
    }
    /// SharedRegion 원본 — sub_view 추가 / heartbeat / role 조회용.
    pub fn shared(&self) -> &SharedRegion {
        &self.shared
    }
    /// 자신의 vm_id.
    pub fn vm_id(&self) -> u32 {
        self.vm_id
    }

    /// Publisher 마지막 heartbeat 이후 경과 ns. publisher 가 아직 시작 안 했으면 None.
    pub fn heartbeat_age_ns(&self) -> Option<u64> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        self.shared.heartbeat_age_ns(now)
    }

    /// Symbol 조회 + frame.symbol_idx 갱신 후 SPSC publish. 성공 시 true,
    /// ring full 이면 false.
    ///
    /// # 실패
    /// Symbol intern 실패 (table full / name too long) 시 warn + false 반환.
    pub fn publish_order(
        &self,
        exchange: ExchangeId,
        symbol_name: &str,
        mut frame: OrderFrame,
    ) -> bool {
        let idx = match self.symtab.get_or_intern(exchange, symbol_name) {
            Ok(i) => i,
            Err(e) => {
                warn!(
                    target: "strategy::shm",
                    exchange = ?exchange,
                    symbol = symbol_name,
                    error = %e,
                    "symtab intern failed — order skipped"
                );
                return false;
            }
        };
        frame.symbol_idx = idx;
        frame.exchange_id = hft_shm::exchange_to_u8(exchange);
        self.order.publish(&frame)
    }

    /// 이미 완성된 raw [`OrderFrame`] 을 현재 VM 의 order ring 에 그대로 쓴다.
    ///
    /// symbol intern / exchange_id 보정 없이 caller 가 채운 값을 신뢰한다.
    /// Step 4b 이후 `ShmOrderEgress` 가 adapter + symtab lookup 을 끝낸 뒤 이 경로를
    /// 사용한다.
    pub fn publish_frame(&self, frame: &OrderFrame) -> bool {
        self.order.publish(frame)
    }

    /// 최신 QuoteSnapshot 을 symbol_idx 로 읽는다.
    pub fn read_quote(&self, symbol_idx: u32) -> Option<QuoteSnapshot> {
        self.quote.read(symbol_idx)
    }

    /// 다음 trade 한 건. 없으면 None.
    pub fn try_consume_trade(&mut self) -> Option<TradeFrame> {
        self.trade.try_consume()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MD poller — spin/park 루프로 quote + trade 이벤트를 handler 에 전달
// ─────────────────────────────────────────────────────────────────────────────

/// Strategy 측 MD consumption loop. Trade 는 SPMC ring 에서 pull, Quote 는 이벤트
/// 기반이 아니므로 원하는 symbol 을 호출자가 polling 구간에서 직접 읽어가는 방식.
///
/// 본 poller 는 **trade 에 대한 spin/park 루프만** 제공. Quote 구독이 필요하면
/// handler 안에서 `client.read_quote(idx)` 를 직접 호출.
///
/// ## 백오프
/// - `spin_limit` 횟수만큼 `spin_loop` → 미소비 시 `park_duration` 동안 park.
/// - 전략이 tight latency 를 원하면 `park_duration = Duration::ZERO` 로 돌린다
///   (100% CPU busy-wait).
pub struct StrategyMdPoller<'a> {
    client: &'a mut StrategyShmClient,
    stop: &'a AtomicBool,
    spin_limit: u32,
    park_duration: Duration,
}

impl<'a> StrategyMdPoller<'a> {
    /// 생성.
    pub fn new(
        client: &'a mut StrategyShmClient,
        stop: &'a AtomicBool,
        spin_limit: u32,
        park_duration: Duration,
    ) -> Self {
        Self {
            client,
            stop,
            spin_limit,
            park_duration,
        }
    }

    /// 주 루프. `on_trade` 가 false 반환 시 break.
    pub fn run<F>(&mut self, mut on_trade: F)
    where
        F: FnMut(&TradeFrame) -> bool,
    {
        let mut spin: u32 = 0;
        while !self.stop.load(Ordering::Acquire) {
            match self.client.try_consume_trade() {
                Some(t) => {
                    spin = 0;
                    if !on_trade(&t) {
                        break;
                    }
                }
                None => {
                    spin = spin.saturating_add(1);
                    if spin < self.spin_limit {
                        std::hint::spin_loop();
                    } else if !self.park_duration.is_zero() {
                        std::thread::park_timeout(self.park_duration);
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_shm::{OrderKind, SharedRegion};
    use tempfile::tempdir;

    fn spec(n: u32) -> LayoutSpec {
        LayoutSpec {
            quote_slot_count: 16,
            trade_ring_capacity: 32,
            symtab_capacity: 16,
            order_ring_capacity: 32,
            n_max: n,
        }
    }

    fn boot_publisher(path: &std::path::Path, s: LayoutSpec) -> SharedRegion {
        let sr = SharedRegion::create_or_attach(
            Backing::DevShm {
                path: path.to_path_buf(),
            },
            s,
            Role::Publisher,
        )
        .unwrap();
        // Strategy attach 는 quote/trade/symtab/order ring 이 모두 초기화됐다고
        // 가정하므로, 테스트 helper 도 publisher 부트 순서를 그대로 따른다.
        let _ = hft_shm::QuoteSlotWriter::from_region(
            sr.sub_region(SubKind::Quote).unwrap(),
            s.quote_slot_count,
        )
        .unwrap();
        let _ = hft_shm::TradeRingWriter::from_region(
            sr.sub_region(SubKind::Trade).unwrap(),
            s.trade_ring_capacity,
        )
        .unwrap();
        let _ = hft_shm::SymbolTable::from_region(
            sr.sub_region(SubKind::Symtab).unwrap(),
            s.symtab_capacity,
        )
        .unwrap();
        // 각 order ring header 초기화.
        for vm_id in 0..s.n_max {
            let sub = sr.sub_region(SubKind::OrderRing { vm_id }).unwrap();
            let _ = OrderRingWriter::from_region(sub, s.order_ring_capacity).unwrap();
        }
        sr
    }

    fn sample_frame(client_id: u64) -> OrderFrame {
        OrderFrame {
            seq: 0,
            kind: OrderKind::Place as u8,
            exchange_id: 0,
            _pad1: [0; 2],
            symbol_idx: 0,
            side: 0,
            tif: 0,
            ord_type: 0,
            _pad2: [0; 1],
            price: 1000,
            size: 1,
            client_id,
            ts_ns: 0,
            aux: [0; 5],
            _pad3: [0; 16],
        }
    }

    #[test]
    fn attach_opens_all_four_handles() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sr1");
        let s = spec(4);
        let _pub = boot_publisher(&path, s);

        let client =
            StrategyShmClient::attach(Backing::DevShm { path: path.clone() }, s, 2).unwrap();
        assert_eq!(client.vm_id(), 2);
        assert_eq!(client.shared().role(), Role::Strategy { vm_id: 2 });
    }

    #[test]
    fn publish_order_intern_and_writes_to_correct_ring() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sr2");
        let s = spec(3);
        let sr_pub = boot_publisher(&path, s);

        let client =
            StrategyShmClient::attach(Backing::DevShm { path: path.clone() }, s, 1).unwrap();
        assert!(client.publish_order(ExchangeId::Gate, "BTC_USDT", sample_frame(42)));

        // publisher 쪽에서 자기 ring 만 consume 해 확인.
        let sub = sr_pub.sub_region(SubKind::OrderRing { vm_id: 1 }).unwrap();
        let mut reader = hft_shm::OrderRingReader::from_region(sub).unwrap();
        let got = reader.try_consume().expect("one frame");
        assert_eq!(got.client_id, 42);

        // 다른 ring 은 비어 있어야 함.
        for other_vm in [0u32, 2] {
            let sub = sr_pub
                .sub_region(SubKind::OrderRing { vm_id: other_vm })
                .unwrap();
            let mut r = hft_shm::OrderRingReader::from_region(sub).unwrap();
            assert!(r.try_consume().is_none(), "vm {} should be empty", other_vm);
        }
    }

    #[test]
    fn publish_frame_writes_raw_frame_without_symbol_rewrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sr2_raw");
        let s = spec(2);
        let sr_pub = boot_publisher(&path, s);

        let client =
            StrategyShmClient::attach(Backing::DevShm { path: path.clone() }, s, 0).unwrap();

        let mut frame = sample_frame(77);
        frame.exchange_id = hft_shm::exchange_to_u8(ExchangeId::Bybit);
        frame.symbol_idx = 9;
        assert!(client.publish_frame(&frame));

        let sub = sr_pub.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap();
        let mut reader = hft_shm::OrderRingReader::from_region(sub).unwrap();
        let got = reader.try_consume().expect("one raw frame");
        assert_eq!(got.client_id, 77);
        assert_eq!(got.exchange_id, hft_shm::exchange_to_u8(ExchangeId::Bybit));
        assert_eq!(got.symbol_idx, 9);
    }

    #[test]
    fn vm_id_out_of_range_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sr3");
        let s = spec(2);
        let _pub = boot_publisher(&path, s);

        let err = StrategyShmClient::attach(
            Backing::DevShm { path: path.clone() },
            s,
            2, // n_max=2 → vm_id=2 는 out of range.
        );
        assert!(err.is_err());
    }

    #[test]
    fn heartbeat_age_reflects_publisher_touch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sr4");
        let s = spec(2);
        let sr_pub = boot_publisher(&path, s);

        let client =
            StrategyShmClient::attach(Backing::DevShm { path: path.clone() }, s, 0).unwrap();
        // touch 전엔 None.
        assert!(client.heartbeat_age_ns().is_none());

        // publisher 가 touch → age 는 Some(작은 값).
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        sr_pub.touch_heartbeat(now);

        let age = client.heartbeat_age_ns().expect("some");
        assert!(age < 1_000_000_000, "age should be < 1s, was {age}");
    }
}
