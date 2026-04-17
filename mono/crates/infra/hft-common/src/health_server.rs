//! health + metrics HTTP 서버.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use hft_telemetry::{counters_snapshot, gauge_get, prometheus::render_prometheus, GaugeKey};

/// health + metrics HTTP 서버 설정.
pub struct HealthServerConfig {
    /// 바인드 포트. 0 이면 OS 가 임의 포트를 배정한다.
    pub port: u16,
    /// 서비스 이름 (/health 응답에 포함).
    pub service_name: String,
}

/// 서버 핸들. cancel 로 종료할 수 있다.
pub struct HealthServerHandle {
    pub local_addr: SocketAddr,
    task: JoinHandle<()>,
}

impl HealthServerHandle {
    /// 서버 태스크 종료를 기다린다.
    pub async fn join(self) {
        let _ = self.task.await;
    }
}

#[derive(Clone)]
struct HealthState {
    service_name: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: String,
    uptime_s: i64,
    counters: BTreeMap<String, u64>,
}

fn selected_health_counters() -> BTreeMap<String, u64> {
    let all: BTreeMap<String, u64> = counters_snapshot().into_iter().collect();
    [
        "supervisor_restart",
        "strategy_gateway_stale",
        "gateway_heartbeat_emitted",
    ]
    .into_iter()
    .map(|key| (key.to_string(), all.get(key).copied().unwrap_or(0)))
    .collect()
}

async fn health(State(state): State<HealthState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: state.service_name,
        uptime_s: gauge_get(GaugeKey::UptimeSeconds),
        counters: selected_health_counters(),
    })
}

async fn metrics() -> Response {
    let body = render_prometheus();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// health + metrics HTTP 서버를 시작한다.
pub async fn start_health_server(
    config: HealthServerConfig,
    cancel: CancellationToken,
) -> Result<HealthServerHandle> {
    let state = HealthState {
        service_name: config.service_name,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .with_state(state);
    let bind_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port));
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("health server bind {bind_addr}"))?;
    let local_addr = listener.local_addr().context("health server local_addr")?;

    let task = tokio::spawn(async move {
        let shutdown = async move {
            cancel.cancelled().await;
        };
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
        {
            warn!(error = %e, "health server stopped with error");
        } else {
            info!("health server stopped");
        }
    });

    Ok(HealthServerHandle { local_addr, task })
}

#[cfg(test)]
mod tests {
    use super::{start_health_server, HealthServerConfig};
    use hft_telemetry::{counter_inc, gauge_set, CounterKey, GaugeKey};
    use tokio_util::sync::CancellationToken;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_endpoint_returns_ok() {
        gauge_set(GaugeKey::UptimeSeconds, 42);
        counter_inc(CounterKey::SupervisorRestart);
        let cancel = CancellationToken::new();
        let handle = start_health_server(
            HealthServerConfig {
                port: 0,
                service_name: "strategy".into(),
            },
            cancel.clone(),
        )
        .await
        .expect("start health server");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let url = format!("http://127.0.0.1:{}/health", handle.local_addr.port());
        let resp = reqwest::get(&url).await.expect("health request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.expect("health json");
        assert_eq!(body["status"], "ok");
        assert_eq!(body["service"], "strategy");
        assert!(body["uptime_s"].as_i64().unwrap_or(-1) >= 0);

        cancel.cancel();
        handle.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_endpoint_returns_prometheus_format() {
        gauge_set(GaugeKey::UptimeSeconds, 7);
        let cancel = CancellationToken::new();
        let handle = start_health_server(
            HealthServerConfig {
                port: 0,
                service_name: "order-gateway".into(),
            },
            cancel.clone(),
        )
        .await
        .expect("start health server");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let url = format!("http://127.0.0.1:{}/metrics", handle.local_addr.port());
        let resp = reqwest::get(&url).await.expect("metrics request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .expect("content type")
            .to_str()
            .expect("content type str")
            .to_string();
        let body = resp.text().await.expect("metrics body");
        assert!(content_type.contains("text/plain"));
        assert!(body.contains("hft_"));

        cancel.cancel();
        handle.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn server_shutdown_on_cancel() {
        let cancel = CancellationToken::new();
        let handle = start_health_server(
            HealthServerConfig {
                port: 0,
                service_name: "strategy".into(),
            },
            cancel.clone(),
        )
        .await
        .expect("start health server");

        cancel.cancel();
        handle.join().await;
    }
}
