//! Order egress 설정.
//!
//! request egress 만 다루며, 결과/ack 역방향 경로는 포함하지 않는다.
//! 정상 경로는 SHM, ZMQ 는 fallback 용으로만 사용한다.

use serde::{Deserialize, Serialize};

use crate::{ConfigError, ConfigResult};

/// 전략 프로세스의 주문 egress 설정.
///
/// `ShmConfig` 가 shared region 경로/role/vm_id 를 담당하고, 이 구조체는
/// "어느 경로를 쓸지" 와 "주문 경로 자체의 튜닝" 만 담당한다.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OrderEgressConfig {
    /// 정상 경로(SHM) 또는 fallback(ZMQ) 선택.
    pub mode: OrderEgressMode,
    /// SHM egress 전용 튜닝.
    pub shm: ShmOrderEgressConfig,
    /// ZMQ fallback 설정. `mode=Zmq` 일 때만 필수.
    pub zmq: Option<ZmqOrderEgressConfig>,
    /// 링 full / downstream 정체 시 처리 정책.
    pub backpressure: BackpressurePolicy,
}

impl Default for OrderEgressConfig {
    fn default() -> Self {
        Self {
            mode: OrderEgressMode::default(),
            shm: ShmOrderEgressConfig::default(),
            zmq: None,
            backpressure: BackpressurePolicy::default(),
        }
    }
}

impl OrderEgressConfig {
    /// 설정값 sanity check.
    pub fn validate(&self) -> ConfigResult<()> {
        macro_rules! ensure {
            ($cond:expr, $msg:expr) => {
                if !$cond {
                    return Err(ConfigError::Invalid($msg.to_string()));
                }
            };
        }

        ensure!(
            self.shm.ring_capacity > 0,
            "order_egress.shm.ring_capacity must be > 0"
        );
        ensure!(
            self.shm.ring_capacity.is_power_of_two(),
            "order_egress.shm.ring_capacity must be a power of two"
        );
        ensure!(
            self.shm.ring_capacity <= (1 << 20),
            "order_egress.shm.ring_capacity must be <= 1<<20"
        );

        if matches!(self.mode, OrderEgressMode::Zmq) {
            ensure!(
                self.zmq.is_some(),
                "order_egress.zmq must be set when order_egress.mode=zmq"
            );
        }

        if let Some(zmq) = &self.zmq {
            ensure!(
                !zmq.endpoint.is_empty(),
                "order_egress.zmq.endpoint must not be empty"
            );
            ensure!(
                zmq.send_hwm > 0,
                "order_egress.zmq.send_hwm must be > 0"
            );
            ensure!(
                zmq.linger_ms >= 0,
                "order_egress.zmq.linger_ms must be >= 0"
            );
            ensure!(
                zmq.reconnect_interval_ms >= 0,
                "order_egress.zmq.reconnect_interval_ms must be >= 0"
            );
            ensure!(
                zmq.reconnect_interval_max_ms >= zmq.reconnect_interval_ms,
                "order_egress.zmq.reconnect_interval_max_ms must be >= reconnect_interval_ms"
            );
        }

        match self.backpressure {
            BackpressurePolicy::Drop => {}
            BackpressurePolicy::RetryWithTimeout {
                max_retries,
                backoff_ns,
                total_timeout_ns,
            } => {
                ensure!(
                    max_retries > 0,
                    "order_egress.backpressure.max_retries must be > 0"
                );
                ensure!(
                    backoff_ns > 0,
                    "order_egress.backpressure.backoff_ns must be > 0"
                );
                ensure!(
                    total_timeout_ns > 0,
                    "order_egress.backpressure.total_timeout_ns must be > 0"
                );
                let min_budget = u64::from(max_retries).saturating_mul(backoff_ns);
                ensure!(
                    total_timeout_ns >= min_budget,
                    "order_egress.backpressure.total_timeout_ns must be >= max_retries * backoff_ns"
                );
            }
            BackpressurePolicy::BlockWithTimeout { timeout_ns } => {
                ensure!(
                    timeout_ns > 0,
                    "order_egress.backpressure.timeout_ns must be > 0"
                );
            }
        }

        Ok(())
    }
}

/// 주문 request egress transport 선택.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OrderEgressMode {
    /// 정상 경로. ivshmem/shared-region 기반 order ring 사용.
    #[default]
    Shm,
    /// Fallback 경로. TCP 위의 ZMQ PUSH 사용.
    Zmq,
}

/// SHM 기반 주문 egress 튜닝.
///
/// shared path / role / vm_id 는 기존 [`crate::ShmConfig`] 에서 재사용하고,
/// 여기서는 order ring 선택과 용량만 다룬다.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ShmOrderEgressConfig {
    /// N 개의 SPSC ring 중 이 전략이 쓰는 ring index.
    pub strategy_ring_id: u32,
    /// ring capacity. 2의 거듭제곱이어야 한다.
    pub ring_capacity: usize,
}

impl Default for ShmOrderEgressConfig {
    fn default() -> Self {
        Self {
            strategy_ring_id: 0,
            ring_capacity: 1024,
        }
    }
}

/// ZMQ fallback 전용 설정.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ZmqOrderEgressConfig {
    /// gateway 수신 endpoint.
    pub endpoint: String,
    /// send-side high-water mark.
    pub send_hwm: i32,
    /// close 시 linger 시간(ms).
    pub linger_ms: i32,
    /// 재연결 기본 간격(ms).
    pub reconnect_interval_ms: i32,
    /// 재연결 최대 간격(ms).
    pub reconnect_interval_max_ms: i32,
}

impl Default for ZmqOrderEgressConfig {
    fn default() -> Self {
        Self {
            endpoint: "tcp://127.0.0.1:7010".into(),
            send_hwm: 1000,
            linger_ms: 0,
            reconnect_interval_ms: 100,
            reconnect_interval_max_ms: 1000,
        }
    }
}

/// backpressure 처리 정책.
///
/// 기본값은 `Drop`.
/// HFT 특성상 stale order 를 늦게 재전송하는 것보다 drop + counter 가 안전하다.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BackpressurePolicy {
    /// 즉시 포기하고 counter 만 증가.
    #[default]
    Drop,
    /// bounded retry 후 포기.
    #[serde(rename = "retry")]
    RetryWithTimeout {
        /// 최대 재시도 횟수.
        max_retries: u32,
        /// 재시도 간 고정 backoff(ns).
        backoff_ns: u64,
        /// 전체 허용 시간(ns).
        total_timeout_ns: u64,
    },
    /// 제한 시간 안에서 block/spin/park 허용.
    #[serde(rename = "block")]
    BlockWithTimeout {
        /// 최대 대기 시간(ns).
        timeout_ns: u64,
    },
}
