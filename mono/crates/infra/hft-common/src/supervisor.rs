//! 공용 soft-restart supervisor.
//!
//! 프로세스 내부에서 recoverable error 를 자동 재시도할 때 사용한다.
//! hard crash (SIGSEGV, OOM, abort) 는 systemd `Restart=on-failure` 가 담당한다.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use hft_telemetry::{counter_inc, CounterKey};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// 서비스 내부 restart 정책.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartConfig {
    /// 최대 연속 실패 허용 횟수.
    pub max_retries: u32,
    /// 초기 backoff.
    pub initial_backoff: Duration,
    /// 최대 backoff.
    pub max_backoff: Duration,
    /// 이 시간 이상 정상 실행되면 연속 실패 카운터를 리셋한다.
    pub healthy_threshold: Duration,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            healthy_threshold: Duration::from_secs(300),
        }
    }
}

/// supervisor 종료 사유.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorExit {
    /// signal 등으로 인한 정상 종료.
    Clean,
    /// 연속 실패 한도 초과.
    MaxRetriesExceeded { attempts: u32, last_error: String },
}

/// `n` 번째 연속 실패 후의 재시작 대기 시간을 계산한다.
#[inline]
fn restart_delay(config: RestartConfig, consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.saturating_sub(1).min(31);
    let factor = 1u32 << shift;
    config
        .initial_backoff
        .checked_mul(factor)
        .unwrap_or(config.max_backoff)
        .min(config.max_backoff)
}

/// 프로세스 내부 soft-restart supervisor.
///
/// `run_fn` 이 `Ok(())` 로 정상 종료하면 loop 를 탈출한다.
/// `Err(..)` 이면 exponential backoff 후 재시작한다.
/// backoff 대기 중에는 `cancel` 을 즉시 반영한다.
pub async fn run_with_restart<F, Fut>(
    service_name: &str,
    config: RestartConfig,
    cancel: CancellationToken,
    run_fn: F,
) -> Result<SupervisorExit>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut consecutive_failures = 0u32;

    loop {
        if cancel.is_cancelled() {
            info!(service = service_name, "supervisor cancelled before service start");
            return Ok(SupervisorExit::Clean);
        }

        let start = Instant::now();
        let result = run_fn().await;

        match result {
            Ok(()) => {
                info!(service = service_name, "service exited cleanly");
                return Ok(SupervisorExit::Clean);
            }
            Err(e) => {
                let elapsed = start.elapsed();
                if elapsed >= config.healthy_threshold {
                    consecutive_failures = 0;
                }
                consecutive_failures = consecutive_failures.saturating_add(1);
                let last_error = format!("{e:#}");

                if consecutive_failures > config.max_retries {
                    error!(
                        service = service_name,
                        attempts = consecutive_failures,
                        error = %last_error,
                        "supervisor max retries exceeded"
                    );
                    return Ok(SupervisorExit::MaxRetriesExceeded {
                        attempts: consecutive_failures,
                        last_error,
                    });
                }

                let delay = restart_delay(config, consecutive_failures);
                counter_inc(CounterKey::SupervisorRestart);
                warn!(
                    service = service_name,
                    consecutive_failures,
                    max_retries = config.max_retries,
                    delay_ms = delay.as_millis() as u64,
                    error = %last_error,
                    "service failed — restarting after backoff"
                );

                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!(service = service_name, "supervisor cancelled during backoff");
                        return Ok(SupervisorExit::Clean);
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn counter_value(key: &str) -> u64 {
        hft_telemetry::counters_snapshot()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_clean_exit() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();
        let out = run_with_restart(
            "test-clean",
            RestartConfig::default(),
            cancel,
            || async { Ok(()) },
        )
        .await
        .expect("supervisor");
        assert_eq!(out, SupervisorExit::Clean);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retry_then_succeed() {
        let _guard = TEST_LOCK.lock().await;
        let before = counter_value("supervisor_restart");
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_run = attempts.clone();
        let cancel = CancellationToken::new();
        let out = run_with_restart(
            "test-retry",
            RestartConfig {
                initial_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(5),
                ..RestartConfig::default()
            },
            cancel,
            move || {
                let attempts = attempts_for_run.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        anyhow::bail!("transient-{n}");
                    }
                    Ok(())
                }
            },
        )
        .await
        .expect("supervisor");
        let after = counter_value("supervisor_restart");
        assert_eq!(out, SupervisorExit::Clean);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(after - before, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_max_retries_exceeded() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();
        let out = run_with_restart(
            "test-max",
            RestartConfig {
                max_retries: 2,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(1),
                ..RestartConfig::default()
            },
            cancel,
            || async { anyhow::bail!("always fail") },
        )
        .await
        .expect("supervisor");
        match out {
            SupervisorExit::MaxRetriesExceeded { attempts, .. } => assert_eq!(attempts, 3),
            other => panic!("unexpected exit: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cancel_during_backoff() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            run_with_restart(
                "test-cancel",
                RestartConfig {
                    initial_backoff: Duration::from_secs(10),
                    max_backoff: Duration::from_secs(10),
                    ..RestartConfig::default()
                },
                cancel_for_task,
                || async { anyhow::bail!("fail once") },
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let out = task.await.expect("join").expect("supervisor");
        assert_eq!(out, SupervisorExit::Clean);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_healthy_resets_counter() {
        let _guard = TEST_LOCK.lock().await;
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_run = attempts.clone();
        let cancel = CancellationToken::new();
        let out = run_with_restart(
            "test-healthy",
            RestartConfig {
                max_retries: 2,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(1),
                healthy_threshold: Duration::from_millis(10),
            },
            cancel,
            move || {
                let attempts = attempts_for_run.clone();
                async move {
                    match attempts.fetch_add(1, Ordering::SeqCst) {
                        0 | 1 => anyhow::bail!("early failure"),
                        2 => {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            anyhow::bail!("late failure")
                        }
                        _ => Ok(()),
                    }
                }
            },
        )
        .await
        .expect("supervisor");
        assert_eq!(out, SupervisorExit::Clean);
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }
}
