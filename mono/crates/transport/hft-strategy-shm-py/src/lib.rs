//! hft-strategy-shm-py — PyO3 binding for [`hft_strategy_shm::StrategyShmClient`].
//!
//! # 역할
//!
//! Strategy VM 이 **Python 에서도** 단일 함수 호출로 SharedRegion 에 붙어
//! `(quote reader, trade reader, symtab, 자기 vm_id 의 order writer)` 네 핸들을
//! 한 번에 얻을 수 있도록 PyO3 래퍼를 제공.
//!
//! # 설계 원칙
//!
//! 1. **ctypes-compatible wire layout 보존** — `OrderFrame` 의 `#[repr(C, align(64))]`
//!    레이아웃은 Rust 측에서 유지하고, Python 은 필드 단위 setter/getter 로
//!    접근한다. Python 측에서 `ctypes.Structure` 를 따로 정의해 직접 쓰기
//!    원한다면 `OrderFrame::SIZE` 와 `OrderFrame::ALIGN` 상수를 export 해 주므로
//!    그에 맞춰 빌드하면 된다.
//! 2. **hot path 에서 파이썬 객체 재사용** — `PyOrderBuilder` 는 한 번 만들고
//!    field 만 갈아끼워 재사용 가능. 매 주문마다 `dict` 를 새로 만드는 비용을
//!    피한다.
//! 3. **GIL release on I/O** — `publish_order`, `try_consume_trade`, `read_quote`
//!    은 전부 native 단순 atomic / memcpy 연산이라 GIL 해제 없이 둠 (전환
//!    비용 > 실제 작업 비용). `attach_with_retry` 만 `allow_threads` 로 sleep
//!    루프 구간에서 GIL 해제.
//! 4. **한 strategy VM = 한 [`PyStrategyClient`]** — `publish_order` 는 `&self`
//!    이지만 `try_consume_trade` 는 SPMC cursor 진행을 위해 `&mut self`.
//!    Python 에선 단일 스레드 사용이 전제 (strategy main loop).
//!
//! # 에러 처리
//!
//! 모든 실패는 `RuntimeError` 로 변환 (`anyhow!` → `PyRuntimeError`). digest
//! mismatch / vm_id 범위 초과 / ring full 등은 각각 `Err` / `False` 로 구분해
//! Python 쪽이 처리하기 쉽게 한다.
//!
//! # Python 사용 예
//!
//! ```python
//! import hft_strategy_shm as shm
//!
//! client = shm.PyStrategyClient.open_pci_bar(
//!     "/sys/bus/pci/devices/0000:00:04.0/resource2",
//!     quote_slot_count=10_000, trade_ring_capacity=1 << 20,
//!     symtab_capacity=16_384, order_ring_capacity=16_384, n_max=4,
//!     vm_id=2,
//! )
//!
//! b = shm.PyOrderBuilder()
//! b.exchange = "gate"
//! b.symbol = "BTC_USDT"
//! b.side = shm.SIDE_BUY
//! b.tif = shm.TIF_IOC
//! b.ord_type = shm.ORD_TYPE_LIMIT
//! b.price_raw = 50_000_00000  # raw i64
//! b.size_raw = 10
//! b.client_id = 42
//! b.ts_ns = shm.wall_clock_ns()
//!
//! ok = client.publish_order(b)
//! ```

#![deny(rust_2018_idioms)]
// PyO3 `#[pymethods]` 가 생성하는 래퍼에서 `PyResult<T>` 변환이 중복으로 보이는
// false positive 가 반복 발생한다. 바인딩 계층 전용 crate 이므로 이 lint 만 한정 허용한다.
#![allow(clippy::useless_conversion)]
#![warn(missing_docs)]

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyType;

use hft_shm::{
    exchange_from_u8, exchange_to_u8, Backing, LayoutSpec, OrderFrame, OrderKind, PlaceAuxMeta,
    QuoteSnapshot, TradeFrame, PLACE_LEVEL_CLOSE, PLACE_LEVEL_OPEN,
};
use hft_strategy_shm::StrategyShmClient;
use hft_types::ExchangeId;

// ─────────────────────────────────────────────────────────────────────────────
// Constants exposed to Python — 이름이 Python 쪽 "shm.*" 접두어로 접근됨.
// ─────────────────────────────────────────────────────────────────────────────

/// Buy side.
pub const SIDE_BUY: u8 = 0;
/// Sell side.
pub const SIDE_SELL: u8 = 1;

/// Good-Till-Cancelled.
pub const TIF_GTC: u8 = 0;
/// Immediate-Or-Cancel.
pub const TIF_IOC: u8 = 1;
/// Fill-Or-Kill.
pub const TIF_FOK: u8 = 2;
/// Post only.
pub const TIF_POSTONLY: u8 = 3;
/// Day order.
pub const TIF_DAY: u8 = 4;

/// Limit order type.
pub const ORD_TYPE_LIMIT: u8 = 0;
/// Market order type.
pub const ORD_TYPE_MARKET: u8 = 1;

/// Place new order kind.
pub const ORDER_KIND_PLACE: u8 = 0;
/// Cancel existing order kind.
pub const ORDER_KIND_CANCEL: u8 = 1;
/// Place 메타 level: Open.
pub const PLACE_LEVEL_OPEN_I: u8 = PLACE_LEVEL_OPEN;
/// Place 메타 level: Close.
pub const PLACE_LEVEL_CLOSE_I: u8 = PLACE_LEVEL_CLOSE;

// ─────────────────────────────────────────────────────────────────────────────
// 유틸 — exchange 문자열 ↔ u8
// ─────────────────────────────────────────────────────────────────────────────

/// Python str → (ExchangeId, u8). 알 수 없으면 `ValueError`.
fn parse_exchange(s: &str) -> PyResult<(ExchangeId, u8)> {
    let ex = ExchangeId::parse(s)
        .ok_or_else(|| PyValueError::new_err(format!("unknown exchange: {s:?}")))?;
    Ok((ex, exchange_to_u8(ex)))
}

/// Wall-clock ns (`CLOCK_REALTIME`). Python 에서 `time.time_ns()` 와 동일 타임베이스.
#[pyfunction]
fn wall_clock_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// PyOrderBuilder — OrderFrame 필드 단위 setter 가 있는 재사용 가능한 bag.
// ─────────────────────────────────────────────────────────────────────────────

/// Python 쪽에서 hot path 에 재사용할 주문 필드 홀더.
///
/// 매 주문마다 새 dict 를 만드는 비용을 피하기 위해 `__init__` 한 번 후
/// attribute 만 갈아끼운다. `publish_order` 는 내부적으로 이 값들을 읽어
/// `OrderFrame` 을 composite.
///
/// symbol intern 은 `publish_order` 안에서 수행 (exchange + symbol 문자열로).
#[pyclass(name = "PyOrderBuilder")]
#[derive(Clone, Debug)]
pub struct PyOrderBuilder {
    /// Exchange enum 문자열 ("gate", "binance", ...).
    #[pyo3(get, set)]
    pub exchange: String,
    /// Symbol (예: "BTC_USDT").
    #[pyo3(get, set)]
    pub symbol: String,
    /// OrderKind (`0=Place`, `1=Cancel`).
    #[pyo3(get, set)]
    pub kind: u8,
    /// Side (`0=Buy`, `1=Sell`).
    #[pyo3(get, set)]
    pub side: u8,
    /// TIF (`0=GTC`, `1=IOC`, ...).
    #[pyo3(get, set)]
    pub tif: u8,
    /// OrderType (`0=Limit`, `1=Market`).
    #[pyo3(get, set)]
    pub ord_type: u8,
    /// 가격 raw i64. market 일 때 0.
    #[pyo3(get, set)]
    pub price_raw: i64,
    /// 크기 raw i64 (signed for long/short).
    #[pyo3(get, set)]
    pub size_raw: i64,
    /// 내부 client_id (Python 발행).
    #[pyo3(get, set)]
    pub client_id: u64,
    /// Python timestamp ns.
    #[pyo3(get, set)]
    pub ts_ns: u64,
    /// aux payload (5 x u64) — cancel 의 exchange_order_id 등.
    #[pyo3(get, set)]
    pub aux: [u64; 5],
    /// Place 주문용 level override. `kind=Place` + `meta_enabled=true` 일 때만 사용.
    meta_level: u8,
    /// Place 주문용 reduce_only override.
    meta_reduce_only: bool,
    /// Place 주문용 text_tag override.
    meta_text_tag: String,
    /// builder-level place meta 자동 packing 활성 여부.
    meta_enabled: bool,
}

#[pymethods]
impl PyOrderBuilder {
    /// 빈 builder 생성. 모든 필드 0.
    #[new]
    pub fn new() -> Self {
        Self::default()
    }

    /// 필드 전체를 0 으로 리셋. 재사용 시 유용.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Place 주문 level override 설정.
    fn level(&mut self, level: u8) {
        self.meta_level = level;
        self.meta_enabled = true;
    }

    /// Place 주문 reduce_only override 설정.
    fn reduce_only(&mut self, reduce_only: bool) {
        self.meta_reduce_only = reduce_only;
        self.meta_enabled = true;
    }

    /// Place 주문 text_tag override 설정.
    fn text_tag(&mut self, text_tag: &str) {
        self.meta_text_tag.clear();
        self.meta_text_tag.push_str(text_tag);
        self.meta_enabled = true;
    }

    fn __repr__(&self) -> String {
        format!(
            "PyOrderBuilder(exchange={:?}, symbol={:?}, kind={}, side={}, tif={}, ord_type={}, price_raw={}, size_raw={}, client_id={}, ts_ns={}, meta_enabled={})",
            self.exchange, self.symbol, self.kind, self.side, self.tif, self.ord_type,
            self.price_raw, self.size_raw, self.client_id, self.ts_ns, self.meta_enabled,
        )
    }
}

impl Default for PyOrderBuilder {
    fn default() -> Self {
        Self {
            exchange: String::new(),
            symbol: String::new(),
            kind: ORDER_KIND_PLACE,
            side: SIDE_BUY,
            tif: TIF_GTC,
            ord_type: ORD_TYPE_LIMIT,
            price_raw: 0,
            size_raw: 0,
            client_id: 0,
            ts_ns: 0,
            aux: [0; 5],
            meta_level: PLACE_LEVEL_OPEN,
            meta_reduce_only: false,
            meta_text_tag: String::new(),
            meta_enabled: false,
        }
    }
}

impl PyOrderBuilder {
    fn place_aux(&self, override_meta: Option<(u8, bool, &str)>) -> [u64; 5] {
        if self.kind != ORDER_KIND_PLACE {
            return self.aux;
        }
        if let Some((level, reduce_only, text_tag)) = override_meta {
            return PlaceAuxMeta::from_parts(level, reduce_only, text_tag).pack();
        }
        if self.meta_enabled {
            return PlaceAuxMeta::from_parts(
                self.meta_level,
                self.meta_reduce_only,
                &self.meta_text_tag,
            )
            .pack();
        }
        self.aux
    }

    /// 내부 OrderFrame composite — exchange 는 u8 로 미리 변환.
    fn to_frame(&self, exchange_u8: u8) -> OrderFrame {
        self.to_frame_with_place_meta(exchange_u8, None)
    }

    fn to_frame_with_place_meta(
        &self,
        exchange_u8: u8,
        override_meta: Option<(u8, bool, &str)>,
    ) -> OrderFrame {
        OrderFrame {
            seq: 0,
            kind: self.kind,
            exchange_id: exchange_u8,
            _pad1: [0; 2],
            // symbol_idx 는 publish_order 쪽에서 채움.
            symbol_idx: 0,
            side: self.side,
            tif: self.tif,
            ord_type: self.ord_type,
            _pad2: [0; 1],
            price: self.price_raw,
            size: self.size_raw,
            client_id: self.client_id,
            ts_ns: self.ts_ns,
            aux: self.place_aux(override_meta),
            _pad3: [0; 16],
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PyQuoteSnapshot — read_quote 반환값.
// ─────────────────────────────────────────────────────────────────────────────

/// Quote 최신 스냅샷. publisher 가 `#[repr(C)]` 로 덮어쓴 tmpfs/PCI BAR 슬롯에서
/// torn-read 방지 seqlock 을 거쳐 복사된 결과.
#[pyclass(name = "PyQuoteSnapshot")]
#[derive(Debug, Clone)]
pub struct PyQuoteSnapshot {
    /// seqlock 값 (짝수). 홀수는 torn 상태라 여기 올라올 일이 없다.
    #[pyo3(get)]
    pub seq: u64,
    /// exchange id raw u8 — [`PyStrategyClient::exchange_from_u8`] 으로 문자열화 가능.
    #[pyo3(get)]
    pub exchange_id: u8,
    /// symbol idx (symtab 기준).
    #[pyo3(get)]
    pub symbol_idx: u32,
    /// bid price raw i64.
    #[pyo3(get)]
    pub bid_price: i64,
    /// bid size raw i64.
    #[pyo3(get)]
    pub bid_size: i64,
    /// ask price raw i64.
    #[pyo3(get)]
    pub ask_price: i64,
    /// ask size raw i64.
    #[pyo3(get)]
    pub ask_size: i64,
    /// 거래소 event 시각 ns.
    #[pyo3(get)]
    pub event_ns: u64,
    /// WS recv 시각 ns.
    #[pyo3(get)]
    pub recv_ns: u64,
    /// publish 시각 ns.
    #[pyo3(get)]
    pub pub_ns: u64,
}

impl From<QuoteSnapshot> for PyQuoteSnapshot {
    fn from(q: QuoteSnapshot) -> Self {
        Self {
            seq: q.seq,
            exchange_id: q.exchange_id,
            symbol_idx: q.symbol_idx,
            bid_price: q.bid_price,
            bid_size: q.bid_size,
            ask_price: q.ask_price,
            ask_size: q.ask_size,
            event_ns: q.event_ns,
            recv_ns: q.recv_ns,
            pub_ns: q.pub_ns,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PyTradeFrame — try_consume_trade 반환값.
// ─────────────────────────────────────────────────────────────────────────────

/// Trade 프레임 — SPMC ring 한 소비 결과. 내부적으로 [`TradeFrame`] 에서 필요한
/// 비-atomic 필드만 복사해 Python 쪽으로 건넨다 (atomic `seq` 는 수치만 읽음).
#[pyclass(name = "PyTradeFrame")]
#[derive(Debug, Clone)]
pub struct PyTradeFrame {
    /// writer 가 기록한 seq (저 bit 0 = unused).
    #[pyo3(get)]
    pub seq: u64,
    /// exchange id raw u8.
    #[pyo3(get)]
    pub exchange_id: u8,
    /// symbol idx.
    #[pyo3(get)]
    pub symbol_idx: u32,
    /// 체결가 raw i64.
    #[pyo3(get)]
    pub price: i64,
    /// 체결량 raw (음수 = sell).
    #[pyo3(get)]
    pub size: i64,
    /// trade id (숫자 우선, 비숫자는 해시 fallback).
    #[pyo3(get)]
    pub trade_id: i64,
    /// event ns.
    #[pyo3(get)]
    pub event_ns: u64,
    /// recv ns.
    #[pyo3(get)]
    pub recv_ns: u64,
    /// pub ns.
    #[pyo3(get)]
    pub pub_ns: u64,
    /// flags (bit0=is_internal).
    #[pyo3(get)]
    pub flags: u32,
}

impl From<&TradeFrame> for PyTradeFrame {
    fn from(t: &TradeFrame) -> Self {
        Self {
            seq: t.seq.load(Ordering::Acquire),
            exchange_id: t.exchange_id,
            symbol_idx: t.symbol_idx,
            price: t.price,
            size: t.size,
            trade_id: t.trade_id,
            event_ns: t.event_ns,
            recv_ns: t.recv_ns,
            pub_ns: t.pub_ns,
            flags: t.flags,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PyStrategyClient — SharedRegion 통합 핸들.
// ─────────────────────────────────────────────────────────────────────────────

/// Python 전용 [`StrategyShmClient`] 래퍼.
///
/// - open_* 클래스 메서드로 backing 을 선택해 attach.
/// - `publish_order(builder)` 로 주문 발행.
/// - `read_quote(symbol_idx)` 로 최신 호가.
/// - `try_consume_trade()` 로 trade ring 한 프레임 소비.
/// - `intern_symbol(exchange, symbol) -> int` 로 symbol_idx 미리 캐싱 가능.
#[pyclass(name = "PyStrategyClient", unsendable)]
pub struct PyStrategyClient {
    inner: StrategyShmClient,
}

// PyO3 `#[pymethods]` 가 생성하는 래퍼에서 `PyResult<T>` 변환이 중복으로 보이는
// false positive 가 발생한다. 바인딩 계층 한정으로만 허용한다.
#[allow(clippy::useless_conversion)]
#[pymethods]
impl PyStrategyClient {
    // ───────────────────────────── 생성자 ────────────────────────────────────

    /// `/dev/shm` 백엔드로 attach.
    ///
    /// # 인자
    /// - `path`: 파일 경로 (publisher 가 만든 `/dev/shm/...`).
    /// - `quote_slot_count`, `trade_ring_capacity`, `symtab_capacity`,
    ///   `order_ring_capacity`, `n_max`: publisher 와 동일한 [`LayoutSpec`] 값.
    /// - `vm_id`: `[0, n_max)` 자기 식별자.
    ///
    /// # 실패
    /// - digest mismatch / vm_id 범위 초과 / publisher 미부트 시 `RuntimeError`.
    #[classmethod]
    #[pyo3(signature = (
        path, *, quote_slot_count, trade_ring_capacity, symtab_capacity,
        order_ring_capacity, n_max, vm_id,
    ))]
    #[allow(clippy::too_many_arguments)]
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn open_dev_shm(
        _cls: &Bound<'_, PyType>,
        path: String,
        quote_slot_count: u32,
        trade_ring_capacity: u64,
        symtab_capacity: u32,
        order_ring_capacity: u64,
        n_max: u32,
        vm_id: u32,
    ) -> PyResult<Self> {
        let backing = Backing::DevShm { path: PathBuf::from(path) };
        Self::attach(
            backing,
            quote_slot_count,
            trade_ring_capacity,
            symtab_capacity,
            order_ring_capacity,
            n_max,
            vm_id,
        )
    }

    /// hugetlbfs 백엔드로 attach.
    #[classmethod]
    #[pyo3(signature = (
        path, *, quote_slot_count, trade_ring_capacity, symtab_capacity,
        order_ring_capacity, n_max, vm_id,
    ))]
    #[allow(clippy::too_many_arguments)]
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn open_hugetlbfs(
        _cls: &Bound<'_, PyType>,
        path: String,
        quote_slot_count: u32,
        trade_ring_capacity: u64,
        symtab_capacity: u32,
        order_ring_capacity: u64,
        n_max: u32,
        vm_id: u32,
    ) -> PyResult<Self> {
        let backing = Backing::Hugetlbfs { path: PathBuf::from(path) };
        Self::attach(
            backing,
            quote_slot_count,
            trade_ring_capacity,
            symtab_capacity,
            order_ring_capacity,
            n_max,
            vm_id,
        )
    }

    /// ivshmem PCI BAR 백엔드로 attach (guest VM 전용).
    #[classmethod]
    #[pyo3(signature = (
        path, *, quote_slot_count, trade_ring_capacity, symtab_capacity,
        order_ring_capacity, n_max, vm_id,
    ))]
    #[allow(clippy::too_many_arguments)]
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn open_pci_bar(
        _cls: &Bound<'_, PyType>,
        path: String,
        quote_slot_count: u32,
        trade_ring_capacity: u64,
        symtab_capacity: u32,
        order_ring_capacity: u64,
        n_max: u32,
        vm_id: u32,
    ) -> PyResult<Self> {
        let backing = Backing::PciBar { path: PathBuf::from(path) };
        Self::attach(
            backing,
            quote_slot_count,
            trade_ring_capacity,
            symtab_capacity,
            order_ring_capacity,
            n_max,
            vm_id,
        )
    }

    /// publisher boot race 대비 재시도 attach. `/dev/shm` 전용 간편 helper.
    ///
    /// `timeout_ms` 동안 `step_ms` 간격으로 attach 재시도. 그 사이 Python GIL 을
    /// 릴리즈해 다른 thread 가 진행 가능하게 한다.
    #[classmethod]
    #[pyo3(signature = (
        path, *, quote_slot_count, trade_ring_capacity, symtab_capacity,
        order_ring_capacity, n_max, vm_id, timeout_ms = 10_000, step_ms = 100,
    ))]
    #[allow(clippy::too_many_arguments)]
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn open_dev_shm_with_retry(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        path: String,
        quote_slot_count: u32,
        trade_ring_capacity: u64,
        symtab_capacity: u32,
        order_ring_capacity: u64,
        n_max: u32,
        vm_id: u32,
        timeout_ms: u64,
        step_ms: u64,
    ) -> PyResult<Self> {
        let backing = Backing::DevShm { path: PathBuf::from(path) };
        let spec = LayoutSpec {
            quote_slot_count,
            trade_ring_capacity,
            symtab_capacity,
            order_ring_capacity,
            n_max,
        };
        // GIL release — sleep 루프가 길 수 있음.
        let inner = py
            .allow_threads(|| {
                StrategyShmClient::attach_with_retry(
                    backing,
                    spec,
                    vm_id,
                    Duration::from_millis(timeout_ms),
                    Duration::from_millis(step_ms),
                )
            })
            .map_err(|e| PyRuntimeError::new_err(format!("attach_with_retry: {e}")))?;
        Ok(Self { inner })
    }

    // ───────────────────────────── 조회 ─────────────────────────────────────

    /// 자기 vm_id.
    #[getter]
    fn vm_id(&self) -> u32 {
        self.inner.vm_id()
    }

    /// Publisher heartbeat 이후 경과 ns. 아직 touch 안 됐으면 None.
    fn heartbeat_age_ns(&self) -> Option<u64> {
        self.inner.heartbeat_age_ns()
    }

    /// symtab 에 symbol 을 intern 해 `symbol_idx` 를 반환.
    ///
    /// 같은 `(exchange, symbol)` 조합은 항상 같은 idx. hot path 전 pre-intern
    /// 용으로 유용.
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn intern_symbol(&self, exchange: &str, symbol: &str) -> PyResult<u32> {
        let (_, ex_u8) = parse_exchange(exchange)?;
        let ex = exchange_from_u8(ex_u8).ok_or_else(|| {
            PyValueError::new_err(format!("exchange u8 {ex_u8} out of known range"))
        })?;
        self.inner
            .symtab()
            .get_or_intern(ex, symbol)
            .map_err(|e| PyRuntimeError::new_err(format!("intern: {e}")))
    }

    /// `symbol_idx` 기반 최신 quote. 아직 쓰이지 않은 슬롯이면 None.
    fn read_quote(&self, symbol_idx: u32) -> Option<PyQuoteSnapshot> {
        self.inner.read_quote(symbol_idx).map(PyQuoteSnapshot::from)
    }

    /// trade ring 다음 한 건. 없으면 None.
    fn try_consume_trade(&mut self) -> Option<PyTradeFrame> {
        self.inner
            .try_consume_trade()
            .as_ref()
            .map(PyTradeFrame::from)
    }

    /// `PyOrderBuilder` 의 현재 필드로 주문 publish.
    ///
    /// 반환: ring push 성공=True, ring full=False, symbol intern 실패=False+warn.
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn publish_order(&self, builder: &PyOrderBuilder) -> PyResult<bool> {
        if builder.symbol.is_empty() {
            return Err(PyValueError::new_err("symbol is empty"));
        }
        let (ex, ex_u8) = parse_exchange(&builder.exchange)?;
        let frame = builder.to_frame(ex_u8);
        Ok(self.inner.publish_order(ex, &builder.symbol, frame))
    }

    /// `PyOrderBuilder` + place 메타를 한 번에 받아 publish.
    ///
    /// 기존 `publish_order(builder)` 는 유지하고, 이 메서드는 Place 분기에서만
    /// `aux` 를 `PlaceAuxMeta` 로 자동 packing 해 준다.
    #[pyo3(signature = (builder, *, level, reduce_only, text_tag))]
    #[allow(clippy::useless_conversion)]
    fn publish_order_with_meta(
        &self,
        builder: &PyOrderBuilder,
        level: u8,
        reduce_only: bool,
        text_tag: &str,
    ) -> PyResult<bool> {
        if builder.symbol.is_empty() {
            return Err(PyValueError::new_err("symbol is empty"));
        }
        let (ex, ex_u8) = parse_exchange(&builder.exchange)?;
        let frame = builder.to_frame_with_place_meta(ex_u8, Some((level, reduce_only, text_tag)));
        Ok(self.inner.publish_order(ex, &builder.symbol, frame))
    }

    /// 단순 필드 기반 fast-path publish — builder 객체를 안 만들고 싶을 때.
    ///
    /// 대부분의 전략 루프는 `PyOrderBuilder` 재사용이 깔끔. 이 메서드는 초기
    /// 프로토타입용.
    #[pyo3(signature = (
        exchange, symbol, *, kind, side, tif, ord_type, price_raw, size_raw,
        client_id, ts_ns,
    ))]
    #[allow(clippy::too_many_arguments)]
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn publish_simple(
        &self,
        exchange: &str,
        symbol: &str,
        kind: u8,
        side: u8,
        tif: u8,
        ord_type: u8,
        price_raw: i64,
        size_raw: i64,
        client_id: u64,
        ts_ns: u64,
    ) -> PyResult<bool> {
        if symbol.is_empty() {
            return Err(PyValueError::new_err("symbol is empty"));
        }
        let (ex, ex_u8) = parse_exchange(exchange)?;
        let frame = OrderFrame {
            seq: 0,
            kind,
            exchange_id: ex_u8,
            _pad1: [0; 2],
            symbol_idx: 0,
            side,
            tif,
            ord_type,
            _pad2: [0; 1],
            price: price_raw,
            size: size_raw,
            client_id,
            ts_ns,
            aux: [0; 5],
            _pad3: [0; 16],
        };
        Ok(self.inner.publish_order(ex, symbol, frame))
    }

    /// 클래스레벨 정적 유틸: u8 → 거래소 문자열. Unknown 이면 `ValueError`.
    #[staticmethod]
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn exchange_from_u8(v: u8) -> PyResult<String> {
        exchange_from_u8(v)
            .map(|e| e.as_str().to_string())
            .ok_or_else(|| PyValueError::new_err(format!("unknown exchange u8: {v}")))
    }

    /// 클래스레벨 정적 유틸: 거래소 문자열 → u8.
    #[staticmethod]
    // PyO3 `#[pymethods]` 래퍼가 생성하는 변환 코드에서 false positive 가 나므로 함수 단위로만 허용.
    #[allow(clippy::useless_conversion)]
    fn exchange_to_u8_str(s: &str) -> PyResult<u8> {
        parse_exchange(s).map(|(_, u)| u)
    }
}

impl PyStrategyClient {
    fn attach(
        backing: Backing,
        quote_slot_count: u32,
        trade_ring_capacity: u64,
        symtab_capacity: u32,
        order_ring_capacity: u64,
        n_max: u32,
        vm_id: u32,
    ) -> PyResult<Self> {
        let spec = LayoutSpec {
            quote_slot_count,
            trade_ring_capacity,
            symtab_capacity,
            order_ring_capacity,
            n_max,
        };
        let inner = StrategyShmClient::attach(backing, spec, vm_id)
            .map_err(|e| PyRuntimeError::new_err(format!("attach: {e}")))?;
        Ok(Self { inner })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Module registration
// ─────────────────────────────────────────────────────────────────────────────

/// PyO3 entry. `_core` 서브모듈로 노출하고 python wrapper (`__init__.py`) 가
/// re-export 한다.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // 상수 등록.
    m.add("SIDE_BUY", SIDE_BUY)?;
    m.add("SIDE_SELL", SIDE_SELL)?;
    m.add("TIF_GTC", TIF_GTC)?;
    m.add("TIF_IOC", TIF_IOC)?;
    m.add("TIF_FOK", TIF_FOK)?;
    m.add("TIF_POSTONLY", TIF_POSTONLY)?;
    m.add("TIF_DAY", TIF_DAY)?;
    m.add("ORD_TYPE_LIMIT", ORD_TYPE_LIMIT)?;
    m.add("ORD_TYPE_MARKET", ORD_TYPE_MARKET)?;
    m.add("ORDER_KIND_PLACE", ORDER_KIND_PLACE)?;
    m.add("ORDER_KIND_CANCEL", ORDER_KIND_CANCEL)?;
    m.add("PLACE_LEVEL_OPEN", PLACE_LEVEL_OPEN_I)?;
    m.add("PLACE_LEVEL_CLOSE", PLACE_LEVEL_CLOSE_I)?;

    // OrderFrame wire 크기/정렬 — Python 측 ctypes 검증용.
    m.add("ORDER_FRAME_SIZE", std::mem::size_of::<OrderFrame>())?;
    m.add("ORDER_FRAME_ALIGN", std::mem::align_of::<OrderFrame>())?;

    m.add_function(wrap_pyfunction!(wall_clock_ns, m)?)?;

    m.add_class::<PyOrderBuilder>()?;
    m.add_class::<PyQuoteSnapshot>()?;
    m.add_class::<PyTradeFrame>()?;
    m.add_class::<PyStrategyClient>()?;

    // 버전 메타 — Python 쪽 assert 시 참고.
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    // OrderKind enum 의 discriminant 상수로 재노출 (중복이지만 가독성).
    m.add("ORDER_KIND_PLACE_I", OrderKind::Place as u8)?;
    m.add("ORDER_KIND_CANCEL_I", OrderKind::Cancel as u8)?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — pyo3 가 주로 Python 런타임에서만 돌리므로 rust-only 검증은 빌드/정합성.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_builder_default_is_zeroed_except_discriminants() {
        let b = PyOrderBuilder::default();
        assert_eq!(b.kind, ORDER_KIND_PLACE);
        assert_eq!(b.side, SIDE_BUY);
        assert_eq!(b.tif, TIF_GTC);
        assert_eq!(b.ord_type, ORD_TYPE_LIMIT);
        assert_eq!(b.price_raw, 0);
        assert_eq!(b.size_raw, 0);
        assert_eq!(b.client_id, 0);
        assert_eq!(b.ts_ns, 0);
        assert_eq!(b.aux, [0u64; 5]);
    }

    #[test]
    fn order_builder_into_frame_preserves_fields() {
        let b = PyOrderBuilder {
            exchange: "gate".into(),
            symbol: "BTC_USDT".into(),
            kind: ORDER_KIND_PLACE,
            side: SIDE_SELL,
            tif: TIF_IOC,
            ord_type: ORD_TYPE_LIMIT,
            price_raw: 5_000_000_000,
            size_raw: 10,
            client_id: 42,
            ts_ns: 1_234_567_890,
            aux: [1, 2, 3, 4, 5],
            meta_level: PLACE_LEVEL_OPEN,
            meta_reduce_only: false,
            meta_text_tag: String::new(),
            meta_enabled: false,
        };

        let f = b.to_frame(exchange_to_u8(ExchangeId::Gate));
        assert_eq!(f.kind, ORDER_KIND_PLACE);
        assert_eq!(f.side, SIDE_SELL);
        assert_eq!(f.tif, TIF_IOC);
        assert_eq!(f.ord_type, ORD_TYPE_LIMIT);
        assert_eq!(f.price, 5_000_000_000);
        assert_eq!(f.size, 10);
        assert_eq!(f.client_id, 42);
        assert_eq!(f.ts_ns, 1_234_567_890);
        assert_eq!(f.aux, [1, 2, 3, 4, 5]);
        assert_eq!(f.exchange_id, exchange_to_u8(ExchangeId::Gate));
        // symbol_idx 는 publish_order 안에서 채워짐.
        assert_eq!(f.symbol_idx, 0);
    }

    #[test]
    fn order_builder_into_frame_packs_place_meta_when_requested() {
        let mut b = PyOrderBuilder {
            exchange: "gate".into(),
            symbol: "BTC_USDT".into(),
            ..PyOrderBuilder::default()
        };
        b.level(PLACE_LEVEL_CLOSE);
        b.reduce_only(true);
        b.text_tag("v6");

        let f = b.to_frame(exchange_to_u8(ExchangeId::Gate));
        let meta = PlaceAuxMeta::unpack(&f.aux);
        assert_eq!(meta.wire_level_code(), PLACE_LEVEL_CLOSE);
        assert!(meta.reduce_only());
        assert_eq!(meta.text_tag_str(), "v6");
    }

    #[test]
    fn order_builder_without_meta_preserves_manual_aux() {
        let b = PyOrderBuilder {
            aux: [1, 2, 3, 4, 5],
            ..PyOrderBuilder::default()
        };

        let f = b.to_frame(exchange_to_u8(ExchangeId::Gate));
        assert_eq!(f.aux, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_exchange_accepts_all_canonical_names() {
        for e in ExchangeId::ALL {
            let (parsed, _u) = parse_exchange(e.as_str()).unwrap();
            assert_eq!(parsed, e);
        }
    }

    #[test]
    fn parse_exchange_rejects_unknown() {
        assert!(parse_exchange("ftx").is_err());
        assert!(parse_exchange("").is_err());
    }

    #[test]
    fn quote_snapshot_round_trips_all_fields() {
        let q = QuoteSnapshot {
            seq: 10,
            exchange_id: 1,
            symbol_idx: 7,
            bid_price: 101,
            bid_size: 2,
            ask_price: 102,
            ask_size: 3,
            event_ns: 1000,
            recv_ns: 1100,
            pub_ns: 1200,
        };
        let p = PyQuoteSnapshot::from(q);
        assert_eq!(p.seq, 10);
        assert_eq!(p.symbol_idx, 7);
        assert_eq!(p.bid_price, 101);
        assert_eq!(p.ask_size, 3);
    }

    #[test]
    fn order_frame_wire_constants_match() {
        assert_eq!(std::mem::size_of::<OrderFrame>(), 128);
        assert_eq!(std::mem::align_of::<OrderFrame>(), 64);
    }
}
