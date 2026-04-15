//! ShmPublisher — Worker 가 ZMQ PUSH 와 **동시에** SHM quote/trade 영역에
//! 써주는 sidecar. 동일 이벤트가 두 경로로 나가므로 intra-host 구독자(전략)는
//! SHM 을 읽고, 모니터링/multi-host fan-out 은 ZMQ 경로를 유지.
//!
//! ## Hot path 특성
//! - alloc 0, syscall 0
//! - `SymbolTable::get_or_intern` 은 최초 1회 miss 시에만 intra-process mutex 를
//!   건드림. 이후 재접근은 락 없이 linear scan (보통 64 symbol 미만).
//! - 심볼별 `(exchange_id, symbol_idx)` 매핑은 Worker 가 살아있는 동안
//!   `DashMap<(ExchangeId, Symbol), u32>` 에 캐싱 → intern 은 실질적으로 한 번만.
//!
//! ## 실패 정책
//! - SHM 비활성 (`enabled=false`) → Worker 가 `ShmPublisher::disabled()` 사용,
//!   모든 메서드 no-op.
//! - intern 실패 (table full / symbol too long) → `ShmInternFail` counter + warn,
//!   해당 이벤트는 ZMQ 만 나가고 SHM 은 skip. hot path 를 정지시키지 않는다.

use std::sync::Arc;

use anyhow::{Context, Result};
use dashmap::DashMap;
use hft_shm::{
    exchange_to_u8, Backing, LayoutSpec, QuoteSlotWriter, QuoteUpdate, Role, SharedRegion,
    SubKind, SymbolTable, TradeFrame, TradeRingWriter,
};
use hft_telemetry::{counter_inc, CounterKey};
use hft_types::{BookTicker, ExchangeId, Symbol, Trade};
use tracing::{debug, info, warn};

/// SHM writer 묶음. Worker 는 이걸 `Option<Arc<ShmPublisher>>` 로 들고 다닌다.
///
/// 모든 writer 는 `&self` 만 받으므로 `Arc` 공유 가능 (내부 단일-writer invariant 는
/// "1 프로세스 내에서 Arc 의 clone 이 여럿이어도 동시에 publish 를 호출하지 않는다" 로
/// 유지 — Worker 는 기본 1개).
pub struct ShmPublisher {
    quote: Arc<QuoteSlotWriter>,
    trade: Arc<TradeRingWriter>,
    symtab: Arc<SymbolTable>,
    /// (exchange, symbol) → idx in-process cache. intern 을 피해 linear scan 비용까지 제거.
    cache: DashMap<(ExchangeId, Symbol), u32>,
    /// v2 SharedRegion (있다면) — writer 들의 parent. drop 순서 보장 **및**
    /// publisher liveness heartbeat 갱신용. Legacy 경로에서는 None.
    shared: Option<Arc<SharedRegion>>,
}

impl std::fmt::Debug for ShmPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmPublisher")
            .field("quote_path", &self.quote.path())
            .field("trade_path", &self.trade.path())
            .field("symtab_path", &self.symtab.path())
            .field("cached_symbols", &self.cache.len())
            .finish()
    }
}

impl ShmPublisher {
    /// Writer 3종을 묶는다. 호출자가 사전에 SHM 파일을 `create` 해 둠. (v1 legacy)
    pub fn new(
        quote: Arc<QuoteSlotWriter>,
        trade: Arc<TradeRingWriter>,
        symtab: Arc<SymbolTable>,
    ) -> Self {
        Self {
            quote,
            trade,
            symtab,
            cache: DashMap::new(),
            shared: None,
        }
    }

    /// v2 multi-VM SharedRegion 에서 3종 writer 를 마운트한다.
    ///
    /// Publisher role 로 attach → Quote / Trade / Symtab sub-region 을 각각 열어
    /// writer 를 만든 뒤, 추가로 **N_max 개의 OrderRing 헤더도 초기화** 한다
    /// (gateway 의 strict attach 가 실패하지 않도록 magic 을 먼저 씀). OrderRing
    /// writer 자체는 여기서 쥐지 않음 — strategy VM 각자가 자기 ring 의 writer 를 연다.
    ///
    /// # 실패
    /// - `backing` 이 `PciBar` 면 publisher 는 **올릴 수 없음** (writable 이 아님).
    /// - layout digest 가 기존 파일과 다르면 `ShmError::LayoutMismatch`.
    pub fn from_shared_region(backing: Backing, spec: LayoutSpec) -> Result<Self> {
        if matches!(backing, Backing::PciBar { .. }) {
            anyhow::bail!(
                "publisher cannot use PciBar backing — that's for VM-side strategy clients"
            );
        }
        let sr = SharedRegion::create_or_attach(backing.clone(), spec, Role::Publisher)
            .with_context(|| format!("create_or_attach SharedRegion at {backing:?}"))?;

        // 각 sub-region writer.
        let qw = QuoteSlotWriter::from_region(
            sr.sub_region(SubKind::Quote).context("sub_region(Quote)")?,
            spec.quote_slot_count,
        )
        .map_err(|e| anyhow::anyhow!("QuoteSlotWriter::from_region: {e}"))?;
        let tw = TradeRingWriter::from_region(
            sr.sub_region(SubKind::Trade).context("sub_region(Trade)")?,
            spec.trade_ring_capacity,
        )
        .map_err(|e| anyhow::anyhow!("TradeRingWriter::from_region: {e}"))?;
        let sym = SymbolTable::from_region(
            sr.sub_region(SubKind::Symtab).context("sub_region(Symtab)")?,
            spec.symtab_capacity,
        )
        .map_err(|e| anyhow::anyhow!("SymbolTable::from_region: {e}"))?;

        // 모든 order ring 헤더 **초기화만** — writer handle 은 버린다. gateway 의
        // strict attach 가 magic 검사에서 통과하도록 하기 위함.
        for vm_id in 0..spec.n_max {
            let sub = sr
                .sub_region(SubKind::OrderRing { vm_id })
                .with_context(|| format!("sub_region(OrderRing{{{vm_id}}})"))?;
            let _w = hft_shm::OrderRingWriter::from_region(sub, spec.order_ring_capacity)
                .map_err(|e| anyhow::anyhow!("OrderRingWriter::from_region vm_id={vm_id}: {e}"))?;
            // _w drop → header 는 이미 valid magic 으로 남음. strategy 쪽에서
            // from_region 재호출 시 magic==valid → validate_header 경로로 진입.
        }

        info!(
            target: "publisher::shm",
            n_max = spec.n_max,
            "publisher attached to SharedRegion + initialized {} order rings",
            spec.n_max
        );

        Ok(Self {
            quote: Arc::new(qw),
            trade: Arc::new(tw),
            symtab: Arc::new(sym),
            cache: DashMap::new(),
            shared: Some(Arc::new(sr)),
        })
    }

    /// Publisher liveness heartbeat 갱신. v2 SharedRegion 경로에서만 effect.
    /// Hot path 에서 매 publish 마다 호출되므로 atomic store 2회만 사용.
    #[inline(always)]
    fn touch_hb(&self, now_ns: u64) {
        if let Some(sr) = &self.shared {
            sr.touch_heartbeat(now_ns);
        }
    }

    /// `(exchange, symbol)` → `symbol_idx`. miss 시 symtab 에 intern.
    ///
    /// 실패 (full/too long) 하면 `None` — 호출자는 SHM publish 를 skip.
    #[inline]
    fn intern(&self, exchange: ExchangeId, symbol: &Symbol) -> Option<u32> {
        let key = (exchange, symbol.clone());
        if let Some(e) = self.cache.get(&key) {
            return Some(*e.value());
        }
        match self.symtab.get_or_intern(exchange, symbol.as_str()) {
            Ok(idx) => {
                self.cache.insert(key, idx);
                Some(idx)
            }
            Err(e) => {
                counter_inc(CounterKey::ShmInternFail);
                warn!(
                    target: "publisher::shm",
                    exchange = ?exchange,
                    symbol = %symbol.as_str(),
                    error = %e,
                    "symtab intern failed — SHM publish skipped"
                );
                None
            }
        }
    }

    /// BookTicker 를 SHM quote slot 에 덮어쓴다.
    ///
    /// stamps 는 ZMQ 쪽과 동일하게 ns 로 통일된 값을 사용. 인자로 받음.
    #[inline]
    pub fn publish_bookticker(
        &self,
        bt: &BookTicker,
        event_ns: u64,
        recv_ns: u64,
        pub_ns: u64,
    ) {
        let Some(idx) = self.intern(bt.exchange, &bt.symbol) else {
            return;
        };
        let update = QuoteUpdate {
            exchange_id: exchange_to_u8(bt.exchange),
            bid_price: bt.bid_price.0 as i64,
            bid_size: bt.bid_size.0 as i64,
            ask_price: bt.ask_price.0 as i64,
            ask_size: bt.ask_size.0 as i64,
            event_ns,
            recv_ns,
            pub_ns,
        };
        if let Err(e) = self.quote.publish(idx, &update) {
            counter_inc(CounterKey::ShmQuoteDropped);
            debug!(
                target: "publisher::shm",
                idx = idx,
                error = %e,
                "shm quote publish failed"
            );
        } else {
            counter_inc(CounterKey::ShmQuotePublished);
        }
        // Publisher liveness tick. `pub_ns` 를 재사용 — 이미 wall-clock ns.
        self.touch_hb(pub_ns);
    }

    /// Trade 를 SHM trade ring 에 broadcast.
    #[inline]
    pub fn publish_trade(
        &self,
        tr: &Trade,
        event_ns: u64,
        recv_ns: u64,
        pub_ns: u64,
    ) {
        let Some(idx) = self.intern(tr.exchange, &tr.symbol) else {
            return;
        };
        // TradeFrame 은 `#[repr(C, align(64))]` 128B. seq 는 writer 가 덮어씀.
        let frame = TradeFrame {
            seq: std::sync::atomic::AtomicU64::new(0),
            exchange_id: exchange_to_u8(tr.exchange),
            _pad1: [0; 3],
            symbol_idx: idx,
            price: tr.price.0 as i64,
            size: tr.size.0 as i64,
            trade_id: tr.trade_id,
            event_ns,
            recv_ns,
            pub_ns,
            flags: if tr.is_internal { 1 } else { 0 },
            _pad2: [0; 28],
        };
        self.trade.publish(&frame);
        counter_inc(CounterKey::ShmTradePublished);
        // Publisher liveness tick.
        self.touch_hb(pub_ns);
    }

    /// 캐시된 (exchange, symbol) 수. 테스트/모니터링용.
    pub fn cached_symbols(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::{Price, Size};
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn mk_publisher(dir: &std::path::Path) -> (ShmPublisher, PathBuf, PathBuf, PathBuf) {
        let qp = dir.join("q");
        let tp = dir.join("t");
        let sp = dir.join("s");
        let qw = Arc::new(QuoteSlotWriter::create(&qp, 32).unwrap());
        let tw = Arc::new(TradeRingWriter::create(&tp, 64).unwrap());
        let sym = Arc::new(SymbolTable::open_or_create(&sp, 32).unwrap());
        (ShmPublisher::new(qw, tw, sym), qp, tp, sp)
    }

    fn sample_bt() -> BookTicker {
        BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(50_000.0),
            ask_price: Price(50_001.0),
            bid_size: Size(1.0),
            ask_size: Size(2.0),
            event_time_ms: 0,
            server_time_ms: 0,
        }
    }

    fn sample_trade() -> Trade {
        Trade {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("ETH_USDT"),
            price: Price(2_500.0),
            size: Size(0.5),
            trade_id: 42,
            create_time_s: 0,
            create_time_ms: 0,
            server_time_ms: 0,
            is_internal: false,
        }
    }

    #[test]
    fn bookticker_publish_roundtrip() {
        use hft_shm::QuoteSlotReader;
        let dir = tempdir().unwrap();
        let (p, qp, _tp, _sp) = mk_publisher(dir.path());

        let bt = sample_bt();
        p.publish_bookticker(&bt, 1, 2, 3);

        // reader 열어서 값 확인.
        let reader = QuoteSlotReader::open(&qp).unwrap();
        // intern 결과 idx=0.
        let snap = reader.read(0).unwrap();
        assert_eq!(snap.bid_price, 50_000);
        assert_eq!(snap.ask_price, 50_001);
        assert_eq!(snap.event_ns, 1);
        assert_eq!(snap.recv_ns, 2);
        assert_eq!(snap.pub_ns, 3);
    }

    #[test]
    fn trade_publish_roundtrip() {
        use hft_shm::TradeRingReader;
        let dir = tempdir().unwrap();
        let (p, _qp, tp, _sp) = mk_publisher(dir.path());

        let tr = sample_trade();
        p.publish_trade(&tr, 100, 101, 102);

        let mut reader = TradeRingReader::open(&tp).unwrap();
        let mut got = Vec::new();
        reader.drain_into(&mut got, 10);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].price, 2_500);
        assert_eq!(got[0].trade_id, 42);
        assert_eq!(got[0].event_ns, 100);
    }

    #[test]
    fn intern_cache_prevents_repeated_table_hits() {
        let dir = tempdir().unwrap();
        let (p, _qp, _tp, _sp) = mk_publisher(dir.path());

        let bt = sample_bt();
        let initial = p.symtab.count();
        p.publish_bookticker(&bt, 0, 0, 0);
        let after1 = p.symtab.count();
        p.publish_bookticker(&bt, 0, 0, 0);
        p.publish_bookticker(&bt, 0, 0, 0);
        let after3 = p.symtab.count();

        assert_eq!(initial, 0);
        assert_eq!(after1, 1);
        assert_eq!(after3, 1, "intern must happen only once");
        assert_eq!(p.cached_symbols(), 1);
    }

    #[test]
    fn different_symbols_get_different_slots() {
        use hft_shm::QuoteSlotReader;
        let dir = tempdir().unwrap();
        let (p, qp, _tp, _sp) = mk_publisher(dir.path());

        let mut bt1 = sample_bt();
        bt1.symbol = Symbol::new("BTC_USDT");
        let mut bt2 = sample_bt();
        bt2.symbol = Symbol::new("ETH_USDT");
        bt2.bid_price = Price(2_500.0);

        p.publish_bookticker(&bt1, 0, 0, 0);
        p.publish_bookticker(&bt2, 0, 0, 0);

        let reader = QuoteSlotReader::open(&qp).unwrap();
        let s1 = reader.read(0).unwrap();
        let s2 = reader.read(1).unwrap();
        assert_eq!(s1.bid_price, 50_000);
        assert_eq!(s2.bid_price, 2_500);
    }

}
