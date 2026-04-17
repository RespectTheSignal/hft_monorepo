//! Order egress 설정.
//!
//! request egress 와 선택적 result ZMQ ingress(connect) 를 다룬다.
//! 정상 request 경로는 SHM, ZMQ 는 fallback 또는 역방향 결과 수신용으로만 사용한다.

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
    /// gateway PUSH bind 에 strategy 가 PULL connect 할 result endpoint.
    #[serde(default)]
    pub result_zmq_connect: Option<String>,
    /// gateway heartbeat timeout (ms). 기본 5000ms (5초).
    /// 0 이면 heartbeat 검사 비활성화 (dev/test 용).
    #[serde(default = "default_heartbeat_timeout_ms")]
    pub heartbeat_timeout_ms: u64,
    /// 링 full / downstream 정체 시 처리 정책.
    pub backpressure: BackpressurePolicy,
}

impl Default for OrderEgressConfig {
    fn default() -> Self {
        Self {
            mode: OrderEgressMode::default(),
            shm: ShmOrderEgressConfig::default(),
            zmq: None,
            result_zmq_connect: None,
            heartbeat_timeout_ms: default_heartbeat_timeout_ms(),
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

        if let Some(endpoint) = self.result_zmq_connect.as_deref() {
            ensure!(
                !endpoint.trim().is_empty(),
                "order_egress.result_zmq_connect must not be empty when set"
            );
            ensure!(
                endpoint.starts_with("tcp://") || endpoint.starts_with("inproc://"),
                "order_egress.result_zmq_connect must start with tcp:// or inproc://"
            );
        }

        if self.heartbeat_timeout_ms > 0 && self.heartbeat_timeout_ms < 1000 {
            return Err(ConfigError::Invalid(
                "order_egress.heartbeat_timeout_ms must be 0 (disabled) or >= 1000"
                    .to_string(),
            ));
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
/// 여기서는 order ring 선택만 다룬다.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ShmOrderEgressConfig {
    /// N 개의 SPSC ring 중 이 전략이 쓰는 ring index.
    pub strategy_ring_id: u32,
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

fn default_heartbeat_timeout_ms() -> u64 {
    5000
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_egress_defaults_are_sensible() {
        let cfg = OrderEgressConfig::default();
        assert_eq!(cfg.mode, OrderEgressMode::Shm);
        assert_eq!(cfg.shm.strategy_ring_id, 0);
        assert!(cfg.zmq.is_none());
        assert!(cfg.result_zmq_connect.is_none());
        assert_eq!(cfg.heartbeat_timeout_ms, 5000);
        assert_eq!(cfg.backpressure, BackpressurePolicy::Drop);
    }

    #[test]
    fn order_egress_serde_roundtrip_default() {
        let cfg = OrderEgressConfig::default();
        let toml = toml::to_string(&cfg).expect("serialize default");
        let back: OrderEgressConfig = toml::from_str(&toml).expect("parse default");
        assert_eq!(back, cfg);
    }

    #[test]
    fn order_egress_serde_roundtrip_full() {
        let cfg = OrderEgressConfig {
            mode: OrderEgressMode::Zmq,
            shm: ShmOrderEgressConfig { strategy_ring_id: 7 },
            zmq: Some(ZmqOrderEgressConfig {
                endpoint: "tcp://infra-vm:5560".into(),
                send_hwm: 2048,
                linger_ms: 0,
                reconnect_interval_ms: 50,
                reconnect_interval_max_ms: 500,
            }),
            result_zmq_connect: Some("tcp://127.0.0.1:7060".into()),
            heartbeat_timeout_ms: 5_000,
            backpressure: BackpressurePolicy::RetryWithTimeout {
                max_retries: 4,
                backoff_ns: 1_000,
                total_timeout_ns: 8_000,
            },
        };
        let toml = toml::to_string_pretty(&cfg).expect("serialize full");
        let back: OrderEgressConfig = toml::from_str(&toml).expect("parse full");
        assert_eq!(back, cfg);
    }

    #[test]
    fn order_egress_rejects_unknown_fields() {
        let err = toml::from_str::<OrderEgressConfig>(
            r#"
                mode = "shm"
                unknown_field = 1
            "#,
        )
        .expect_err("unknown field must fail");
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn order_egress_result_zmq_connect_validate() {
        let mut cfg = OrderEgressConfig {
            result_zmq_connect: Some("".into()),
            ..OrderEgressConfig::default()
        };
        assert!(cfg.validate().is_err());

        cfg.result_zmq_connect = Some("ipc://bad".into());
        assert!(cfg.validate().is_err());

        cfg.result_zmq_connect = Some("inproc://strategy-result".into());
        cfg.validate().expect("valid result connect endpoint");
    }

    #[test]
    fn order_egress_heartbeat_timeout_validate() {
        let mut cfg = OrderEgressConfig {
            heartbeat_timeout_ms: 999,
            ..OrderEgressConfig::default()
        };
        assert!(cfg.validate().is_err());

        cfg.heartbeat_timeout_ms = 0;
        cfg.validate().expect("zero disables heartbeat");

        cfg.heartbeat_timeout_ms = 5_000;
        cfg.validate().expect("valid heartbeat timeout");
    }

    #[test]
    fn order_egress_zmq_required_when_mode_zmq() {
        let cfg = OrderEgressConfig {
            mode: OrderEgressMode::Zmq,
            zmq: None,
            ..OrderEgressConfig::default()
        };
        let err = cfg.validate().expect_err("zmq config must be required");
        assert!(err
            .to_string()
            .contains("order_egress.zmq must be set when order_egress.mode=zmq"));
    }

    #[test]
    fn order_egress_backpressure_retry_policy_bounds() {
        let cfg = OrderEgressConfig {
            backpressure: BackpressurePolicy::RetryWithTimeout {
                max_retries: 4,
                backoff_ns: 1_000,
                total_timeout_ns: 3_000,
            },
            ..OrderEgressConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("too-small timeout must fail validation");
        assert!(err
            .to_string()
            .contains("total_timeout_ns must be >= max_retries * backoff_ns"));
    }
}
