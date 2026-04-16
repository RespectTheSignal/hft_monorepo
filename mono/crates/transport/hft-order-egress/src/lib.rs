//! 전략 주문 egress 추상화.
//!
//! Step 4b 에서는 서비스 결선 전에 transport-agnostic interface 를 먼저 세운다.
//! SHM / ZMQ 구현체와 backpressure 정책 wrapper 는 후속 커밋에서 추가된다.

#![deny(rust_2018_idioms)]

mod policy;
mod shm;
mod zmq;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use hft_exchange_api::OrderRequest;
use hft_protocol::{OrderAdaptError, OrderEgressMeta, WireLevel};
use thiserror::Error;

pub use policy::PolicyOrderEgress;
pub use shm::ShmOrderEgress;
pub use zmq::ZmqOrderEgress;

/// one-shot 전송 시도 결과.
///
/// `WouldBlock` 은 에러가 아니라 downstream 이 가득 찬 상태를 뜻한다.
/// 실제 retry / drop / block 정책은 상위 wrapper 가 결정한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// 성공적으로 전송했다.
    Sent,
    /// ring / queue / socket HWM 이 가득 차 즉시 전송할 수 없다.
    WouldBlock,
}

/// 주문 egress 공통 에러.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SubmitError {
    /// 도메인 주문 → wire/frame 적응 실패.
    #[error("adapt failed: {0}")]
    Adapt(#[from] OrderAdaptError),
    /// transport primitive 자체 에러.
    #[error("transport error: {0}")]
    Transport(String),
}

/// transport-agnostic order egress.
///
/// 구현체는 **non-blocking** `try_submit()` 만 제공한다.
/// backpressure 정책은 [`PolicyOrderEgress`] 가 감싼 뒤 적용한다.
pub trait OrderEgress: Send + Sync {
    /// 한 번만 전송을 시도한다.
    fn try_submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError>;
}

/// borrow 기반 [`OrderEgressMeta`] 의 owned 복사본.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedOrderEgressMeta {
    /// 전략 내부 단조 증가 시퀀스.
    pub client_seq: u64,
    /// 전략 태그.
    pub strategy_tag: String,
    /// 주문 레벨.
    pub level: WireLevel,
    /// reduce_only 여부.
    pub reduce_only: bool,
    /// transport 진입 시각.
    pub origin_ts_ns: u64,
    /// ZMQ symbol id.
    pub symbol_id: Option<u32>,
    /// SHM symbol idx.
    pub symbol_idx: Option<u32>,
}

impl From<&OrderEgressMeta<'_>> for OwnedOrderEgressMeta {
    fn from(meta: &OrderEgressMeta<'_>) -> Self {
        Self {
            client_seq: meta.client_seq,
            strategy_tag: meta.strategy_tag.to_string(),
            level: meta.level,
            reduce_only: meta.reduce_only,
            origin_ts_ns: meta.origin_ts_ns,
            symbol_id: meta.symbol_id,
            symbol_idx: meta.symbol_idx,
        }
    }
}

/// 테스트용 기록 레코드.
#[derive(Debug, Clone)]
pub struct RecordedSubmit {
    /// 원본 주문 요청.
    pub req: OrderRequest,
    /// egress 메타데이터 복사본.
    pub meta: OwnedOrderEgressMeta,
}

fn record_submit(
    log: &Mutex<Vec<RecordedSubmit>>,
    req: &OrderRequest,
    meta: &OrderEgressMeta<'_>,
) {
    log.lock().expect("record lock poisoned").push(RecordedSubmit {
        req: req.clone(),
        meta: OwnedOrderEgressMeta::from(meta),
    });
}

/// 항상 성공하는 테스트용 egress.
#[derive(Default)]
pub struct NoopOrderEgress {
    calls: Mutex<Vec<RecordedSubmit>>,
}

impl NoopOrderEgress {
    /// 지금까지 기록된 호출 로그 복사본.
    pub fn recorded(&self) -> Vec<RecordedSubmit> {
        self.calls.lock().expect("calls lock poisoned").clone()
    }
}

impl OrderEgress for NoopOrderEgress {
    fn try_submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        record_submit(&self.calls, req, meta);
        Ok(SubmitOutcome::Sent)
    }
}

/// 항상 `WouldBlock` 을 반환하는 테스트용 egress.
#[derive(Default)]
pub struct BlockingOrderEgress {
    calls: Mutex<Vec<RecordedSubmit>>,
}

impl BlockingOrderEgress {
    /// 지금까지 기록된 호출 로그 복사본.
    pub fn recorded(&self) -> Vec<RecordedSubmit> {
        self.calls.lock().expect("calls lock poisoned").clone()
    }
}

impl OrderEgress for BlockingOrderEgress {
    fn try_submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        record_submit(&self.calls, req, meta);
        Ok(SubmitOutcome::WouldBlock)
    }
}

/// 처음 N번은 `WouldBlock`, 이후에는 `Sent`.
pub struct FlakyOrderEgress {
    remaining_would_block: AtomicU32,
    calls: Mutex<Vec<RecordedSubmit>>,
}

impl FlakyOrderEgress {
    /// `would_block_times` 번 막힌 뒤 성공하는 egress 생성.
    pub fn new(would_block_times: u32) -> Self {
        Self {
            remaining_would_block: AtomicU32::new(would_block_times),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 지금까지 기록된 호출 로그 복사본.
    pub fn recorded(&self) -> Vec<RecordedSubmit> {
        self.calls.lock().expect("calls lock poisoned").clone()
    }
}

impl OrderEgress for FlakyOrderEgress {
    fn try_submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        record_submit(&self.calls, req, meta);
        let remaining = self.remaining_would_block.load(Ordering::SeqCst);
        if remaining > 0 {
            self.remaining_would_block.fetch_sub(1, Ordering::SeqCst);
            Ok(SubmitOutcome::WouldBlock)
        } else {
            Ok(SubmitOutcome::Sent)
        }
    }
}

/// 항상 transport error 를 반환하는 테스트용 egress.
pub struct FailingOrderEgress {
    message: String,
    calls: Mutex<Vec<RecordedSubmit>>,
}

impl FailingOrderEgress {
    /// 지정한 메시지로 항상 실패하는 egress 생성.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 지금까지 기록된 호출 로그 복사본.
    pub fn recorded(&self) -> Vec<RecordedSubmit> {
        self.calls.lock().expect("calls lock poisoned").clone()
    }
}

impl Default for FailingOrderEgress {
    fn default() -> Self {
        Self::new("forced transport failure")
    }
}

impl OrderEgress for FailingOrderEgress {
    fn try_submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        record_submit(&self.calls, req, meta);
        Err(SubmitError::Transport(self.message.clone()))
    }
}
