//! Backpressure policy wrapper.
//!
//! transport 구현체는 `WouldBlock` 만 보고하고, 실제 drop / retry / block 의미는
//! 이 wrapper 가 담당한다.

use std::time::{Duration, Instant};

use hft_config::order_egress::BackpressurePolicy;
use hft_exchange_api::OrderRequest;
use hft_protocol::OrderEgressMeta;
use hft_telemetry::{counter_inc, CounterKey};

use crate::{OrderEgress, SubmitError, SubmitOutcome};

/// backpressure 정책을 적용하는 wrapper.
pub struct PolicyOrderEgress<E: OrderEgress> {
    inner: E,
    policy: BackpressurePolicy,
}

impl<E: OrderEgress> PolicyOrderEgress<E> {
    /// 새 wrapper 생성.
    pub fn new(inner: E, policy: BackpressurePolicy) -> Self {
        Self { inner, policy }
    }

    /// 내부 구현체 접근.
    pub fn inner(&self) -> &E {
        &self.inner
    }

    /// 정책을 적용한 submit.
    pub fn submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        match self.policy.clone() {
            BackpressurePolicy::Drop => self.submit_drop(req, meta),
            BackpressurePolicy::RetryWithTimeout {
                max_retries,
                backoff_ns,
                total_timeout_ns,
            } => self.submit_retry(req, meta, max_retries, backoff_ns, total_timeout_ns),
            BackpressurePolicy::BlockWithTimeout { timeout_ns } => {
                self.submit_block(req, meta, timeout_ns)
            }
        }
    }

    fn submit_drop(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        match self.inner.try_submit(req, meta)? {
            SubmitOutcome::Sent => Ok(SubmitOutcome::Sent),
            SubmitOutcome::WouldBlock => {
                counter_inc(CounterKey::OrderEgressBackpressureDropped);
                Ok(SubmitOutcome::WouldBlock)
            }
        }
    }

    fn submit_retry(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
        max_retries: u32,
        backoff_ns: u64,
        total_timeout_ns: u64,
    ) -> Result<SubmitOutcome, SubmitError> {
        let started = Instant::now();
        let timeout = Duration::from_nanos(total_timeout_ns);
        let mut attempts = 0u32;

        loop {
            match self.inner.try_submit(req, meta)? {
                SubmitOutcome::Sent => return Ok(SubmitOutcome::Sent),
                SubmitOutcome::WouldBlock => {
                    attempts = attempts.saturating_add(1);
                    counter_inc(CounterKey::OrderEgressBackpressureRetried);
                    if attempts >= max_retries || started.elapsed() >= timeout {
                        counter_inc(CounterKey::OrderEgressBackpressureRetryExhausted);
                        return Ok(SubmitOutcome::WouldBlock);
                    }
                    wait_backoff(backoff_ns);
                }
            }
        }
    }

    fn submit_block(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
        timeout_ns: u64,
    ) -> Result<SubmitOutcome, SubmitError> {
        let started = Instant::now();
        let timeout = Duration::from_nanos(timeout_ns);
        let mut blocked = false;

        loop {
            match self.inner.try_submit(req, meta)? {
                SubmitOutcome::Sent => {
                    if blocked {
                        counter_inc(CounterKey::OrderEgressBackpressureBlocked);
                    }
                    return Ok(SubmitOutcome::Sent);
                }
                SubmitOutcome::WouldBlock => {
                    blocked = true;
                    if started.elapsed() >= timeout {
                        counter_inc(CounterKey::OrderEgressBackpressureBlockTimeout);
                        return Ok(SubmitOutcome::WouldBlock);
                    }
                    wait_block_once(timeout_ns);
                }
            }
        }
    }
}

#[inline]
fn wait_backoff(backoff_ns: u64) {
    if backoff_ns < 1_000 {
        std::hint::spin_loop();
    } else {
        std::thread::sleep(Duration::from_nanos(backoff_ns));
    }
}

#[inline]
fn wait_block_once(timeout_ns: u64) {
    if timeout_ns < 1_000 {
        std::hint::spin_loop();
    } else {
        std::thread::yield_now();
    }
}
