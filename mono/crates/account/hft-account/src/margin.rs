//! 크로스 계정 마진 모니터링 (Phase 3: 읽기 전용).
//!
//! 자동 이체/리밸런싱은 범위 밖이며, 이 배치에서는 잔고/포지션 snapshot 집계만 한다.

use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::manager::{AccountHandle, AccountManager};

/// 개별 계정의 마진 스냅샷.
#[derive(Debug, Clone)]
pub struct AccountMarginSnapshot {
    pub login_name: String,
    /// 선물 계정 총 잔고 (USDT).
    pub total_usdt: f64,
    /// 미실현 PnL (USDT).
    pub unrealized_pnl_usdt: f64,
    /// 사용 가능 잔고 (USDT).
    pub available_usdt: f64,
    /// 포지션 수.
    pub position_count: usize,
    /// 수집 성공 여부.
    pub ok: bool,
    /// 실패 시 에러 메시지.
    pub error: Option<String>,
}

/// 전체 계정 집계 요약.
#[derive(Debug, Clone)]
pub struct MarginSummary {
    pub snapshots: Vec<AccountMarginSnapshot>,
    pub total_usdt_sum: f64,
    pub unrealized_pnl_sum: f64,
    pub available_usdt_sum: f64,
    pub total_positions: usize,
    pub failed_count: usize,
}

/// 복수 계정 마진 모니터.
pub struct MarginManager {
    accounts: Vec<AccountHandle>,
}

impl MarginManager {
    /// `AccountManager` 에서 활성 계정을 복사해 초기화한다.
    pub fn new(mgr: &AccountManager) -> Self {
        let accounts: Vec<AccountHandle> = mgr
            .enabled_accounts()
            .iter()
            .map(|handle| (*handle).clone())
            .collect();
        info!(
            target: "hft_account::margin",
            count = accounts.len(),
            "MarginManager initialized"
        );
        Self { accounts }
    }

    /// 모든 계정의 잔고/포지션 snapshot 을 병렬 수집한다.
    pub async fn collect_snapshot(&self) -> MarginSummary {
        let mut set = JoinSet::new();
        for handle in &self.accounts {
            let handle = handle.clone();
            set.spawn(async move { collect_single(&handle).await });
        }

        let mut snapshots = Vec::with_capacity(self.accounts.len());
        while let Some(result) = set.join_next().await {
            match result {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(e) => {
                    warn!(
                        target: "hft_account::margin",
                        error = %e,
                        "margin snapshot task panicked"
                    );
                }
            }
        }

        snapshots.sort_by(|a, b| a.login_name.cmp(&b.login_name));

        let mut summary = MarginSummary {
            snapshots: Vec::new(),
            total_usdt_sum: 0.0,
            unrealized_pnl_sum: 0.0,
            available_usdt_sum: 0.0,
            total_positions: 0,
            failed_count: 0,
        };

        for snapshot in &snapshots {
            if snapshot.ok {
                summary.total_usdt_sum += snapshot.total_usdt;
                summary.unrealized_pnl_sum += snapshot.unrealized_pnl_usdt;
                summary.available_usdt_sum += snapshot.available_usdt;
                summary.total_positions += snapshot.position_count;
            } else {
                summary.failed_count += 1;
            }
        }
        summary.snapshots = snapshots;
        summary
    }

    /// 등록 계정 수.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }
}

async fn collect_single(handle: &AccountHandle) -> AccountMarginSnapshot {
    let login = handle.login_name.clone();

    let balance = match handle.client.fetch_accounts().await {
        Ok(balance) => balance,
        Err(e) => {
            warn!(
                target: "hft_account::margin",
                login = %login,
                error = %e,
                "failed to fetch account balance"
            );
            return AccountMarginSnapshot {
                login_name: login,
                total_usdt: 0.0,
                unrealized_pnl_usdt: 0.0,
                available_usdt: 0.0,
                position_count: 0,
                ok: false,
                error: Some(format!("{e:#}")),
            };
        }
    };

    let position_count = match handle.client.fetch_open_positions().await {
        Ok(positions) => positions.len(),
        Err(e) => {
            warn!(
                target: "hft_account::margin",
                login = %login,
                error = %e,
                "failed to fetch positions — balance only"
            );
            0
        }
    };

    AccountMarginSnapshot {
        login_name: login,
        total_usdt: balance.total_usdt,
        unrealized_pnl_usdt: balance.unrealized_pnl_usdt,
        available_usdt: balance.available_usdt,
        position_count,
        ok: true,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_summary_aggregation() {
        let snapshots = vec![
            AccountMarginSnapshot {
                login_name: "a".into(),
                total_usdt: 1000.0,
                unrealized_pnl_usdt: 50.0,
                available_usdt: 900.0,
                position_count: 3,
                ok: true,
                error: None,
            },
            AccountMarginSnapshot {
                login_name: "b".into(),
                total_usdt: 0.0,
                unrealized_pnl_usdt: 0.0,
                available_usdt: 0.0,
                position_count: 0,
                ok: false,
                error: Some("timeout".into()),
            },
            AccountMarginSnapshot {
                login_name: "c".into(),
                total_usdt: 2000.0,
                unrealized_pnl_usdt: -30.0,
                available_usdt: 1800.0,
                position_count: 5,
                ok: true,
                error: None,
            },
        ];

        let mut summary = MarginSummary {
            snapshots: Vec::new(),
            total_usdt_sum: 0.0,
            unrealized_pnl_sum: 0.0,
            available_usdt_sum: 0.0,
            total_positions: 0,
            failed_count: 0,
        };
        for snapshot in &snapshots {
            if snapshot.ok {
                summary.total_usdt_sum += snapshot.total_usdt;
                summary.unrealized_pnl_sum += snapshot.unrealized_pnl_usdt;
                summary.available_usdt_sum += snapshot.available_usdt;
                summary.total_positions += snapshot.position_count;
            } else {
                summary.failed_count += 1;
            }
        }
        summary.snapshots = snapshots;

        assert_eq!(summary.total_usdt_sum, 3000.0);
        assert_eq!(summary.unrealized_pnl_sum, 20.0);
        assert_eq!(summary.available_usdt_sum, 2700.0);
        assert_eq!(summary.total_positions, 8);
        assert_eq!(summary.failed_count, 1);
        assert_eq!(summary.snapshots.len(), 3);
    }
}
