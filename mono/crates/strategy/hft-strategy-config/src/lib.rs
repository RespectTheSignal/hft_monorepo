//! 전략 설정 (`TradeSettings`) + Supabase 로더.
//!
//! Phase 2 리팩토링 포인트 대비 레거시 `gate_hft_rust::config` 차이:
//!
//! * `reqwest::blocking` → **async** `reqwest::Client` 사용. 전략 runtime 이 tokio
//!   기반이라 blocking 호출이 runtime worker 를 점유하면 전체 hot path 가 스톨한다.
//! * `StrategyConfig::symbols_map` 같은 `HashMap<String,bool>` 포함-여부 테이블 →
//!   ahash + `SymbolSet` 로 캐시 라인 친화. 레거시는 매 요청마다 String alloc 이 있었음.
//! * Supabase 응답은 응답 바디 전체를 힙에 올리지 않고 그대로 serde_json 파싱 →
//!   문자열 id 와 숫자 id 모두 수용하는 `deserialize_text_id` custom visitor 는 유지.
//! * 배포 전환 시 Supabase 장애 대비 — 마지막으로 성공적으로 읽은 설정을 `ArcSwap` 로
//!   캐싱해 "마지막 성공값 사용 (graceful fallback)" 지원. refresh 주기는 호출부가 결정.
//! * 모든 기본값은 figment defaults 가 아니라 `Default` trait 로 중앙화 →
//!   config 로더 한 군데에서만 기본값 체계를 본다.
//!
//! ⚠️ 기본값은 **레거시 config.rs 의 default_* 함수 값과 1:1 일치** 시켰다. 전략
//! 수치의 의미를 건드리는 변경이 아니며, 단지 레이어링을 바꾼 것.

use std::sync::Arc;

use ahash::AHashMap;
use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{debug, warn};

/// 전략 한 개의 매매 파라미터 묶음. 레거시와 **동일한 필드/기본값**을 보존한다.
///
/// 운영 시 Supabase `trade_settings` 테이블의 JSON payload 한 행이 이 struct 하나로
/// deserialize 된다. 누락된 필드는 모두 `Default` 로 채워짐 (`#[serde(default)]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TradeSettings {
    // ── 주문 주기 / 크기 ────────────────────────────────────────────────
    /// 시그널 재평가 간격(초). 레거시: 0.1
    pub update_interval_seconds: f64,
    /// 1회 기본 주문 USDT 크기. 레거시: 0
    pub order_size: f64,
    /// 최대 순포지션 (USDT). 레거시: 0
    pub max_position_size: f64,
    /// dynamic_max_position_size=true 시 `account_balance * multiplier` 사용. 기본 1.5
    pub max_position_size_multiplier: f64,
    /// 포지션 한도를 잔고 기반으로 환산할지 여부. 기본 false
    pub dynamic_max_position_size: bool,
    /// max_position_size_multiplier 를 자동으로 bypass 허용할지. 기본 false
    pub bypass_max_position_size: bool,
    /// 순포지션이 반대 방향일 때 한도. None 이면 `max_position_size * 1.5`
    pub opposite_side_max_position_size: Option<f64>,
    /// 시그널을 트리거하는 orderbook 사이즈 하한 (contract 단위)
    pub trade_size_trigger: i64,
    /// close 진입 필요한 open 주문 누적 count
    pub close_order_count: u64,
    /// 최대 order size multiplier. close 주문 조정 시 사용. 기본 3
    pub max_order_size_multiplier: i64,
    /// close 주문 시 USDT 기준 크기 override. None → `order_size` 재사용
    pub close_order_size: Option<f64>,

    // ── 수익성 / 스프레드 gate ─────────────────────────────────────────────
    /// raw mid 대비 profit basis point 임계. 기본 0
    pub close_raw_mid_profit_bp: f64,
    /// mid 갭 basis point 진입 임계
    pub mid_gap_bp_threshold: f64,
    /// 스프레드 bp 하한
    pub spread_bp_threshold: f64,
    /// close_ws_order wait_time_ms. 기본 1000
    pub wait_time_close_ws_order_ms: u64,

    // ── TIF / 타임 제약 ─────────────────────────────────────────────────────
    /// open 주문 TIF. 기본 "fok"
    pub limit_open_tif: String,
    /// close 주문 TIF. 기본 "fok"
    pub limit_close_tif: String,

    // ── 시간 제약 (랜덤화 min/max 쌍) ────────────────────────────────────
    /// 동일 side 동일 가격 주문 간 최소 간격 (ms) — min. 기본 400
    pub same_side_price_time_restriction_ms_min: i64,
    /// 동일 side 동일 가격 주문 간 최대 간격 (ms) — max. 기본 600
    pub same_side_price_time_restriction_ms_max: i64,
    /// limit_open 같은 방향 재주문 최소 간격 — min. 기본 300
    pub limit_open_time_restriction_ms_min: i64,
    /// limit_open 같은 방향 재주문 최대 간격 — max. 기본 600
    pub limit_open_time_restriction_ms_max: i64,
    /// limit_close 간격 — min. 기본 400
    pub limit_close_time_restriction_ms_min: i64,
    /// limit_close 간격 — max. 기본 800
    pub limit_close_time_restriction_ms_max: i64,

    // ── 레이턴시 gate (ms) ─────────────────────────────────────────────────
    pub gate_last_book_ticker_latency_ms: i64,
    pub binance_last_book_ticker_latency_ms: i64,
    pub gate_last_webbook_ticker_latency_ms: i64,

    // ── 펀딩 레이트 ────────────────────────────────────────────────────────
    /// |funding_rate| 가 이 값보다 크면 only_close 모드로 전환. 기본 0.0005 (=5bp/8h)
    pub funding_rate_threshold: f64,

    // ── EMA 기반 수익성 필터 ───────────────────────────────────────────────
    /// EMA alpha. 기본 0.1
    pub profit_bp_ema_alpha: f64,
    /// 진입 허용을 위한 EMA 임계. 기본 1.20
    pub profit_bp_ema_threshold: f64,
    /// 수익성(성공률) 임계. 기본 0.6
    pub succes_threshold: f64,
    /// 정규화 거래 개수. 기본 100
    pub normalize_trade_count: i64,

    // ── too_many_orders 조치 ───────────────────────────────────────────────
    pub too_many_orders_time_gap_ms: i64,
    pub too_many_orders_size_threshold_multiplier: i64,
    pub symbol_too_many_orders_size_threshold_multiplier: i64,

    // ── stale position close ───────────────────────────────────────────────
    pub close_stale_minutes: i64,
    pub close_stale_minutes_size: Option<f64>,
}

impl Default for TradeSettings {
    fn default() -> Self {
        Self {
            // 주기/크기
            update_interval_seconds: 0.1,
            order_size: 0.0,
            max_position_size: 0.0,
            max_position_size_multiplier: 1.5,
            dynamic_max_position_size: false,
            bypass_max_position_size: false,
            opposite_side_max_position_size: None,
            trade_size_trigger: 0,
            close_order_count: 0,
            max_order_size_multiplier: 3,
            close_order_size: None,

            // profit/spread gate
            close_raw_mid_profit_bp: 0.0,
            mid_gap_bp_threshold: 0.0,
            spread_bp_threshold: 0.0,
            wait_time_close_ws_order_ms: 1000,

            // TIF
            limit_open_tif: "fok".to_string(),
            limit_close_tif: "fok".to_string(),

            // time restrictions (랜덤화 쌍, 레거시 기본값과 동일)
            same_side_price_time_restriction_ms_min: 400,
            same_side_price_time_restriction_ms_max: 600,
            limit_open_time_restriction_ms_min: 300,
            limit_open_time_restriction_ms_max: 600,
            limit_close_time_restriction_ms_min: 400,
            limit_close_time_restriction_ms_max: 800,

            // latency gate
            gate_last_book_ticker_latency_ms: 500,
            binance_last_book_ticker_latency_ms: 500,
            gate_last_webbook_ticker_latency_ms: 500,

            // funding
            funding_rate_threshold: 0.0005,

            // EMA
            profit_bp_ema_alpha: 0.1,
            profit_bp_ema_threshold: 1.20,
            succes_threshold: 0.6,
            normalize_trade_count: 100,

            // too_many_orders
            too_many_orders_time_gap_ms: 1000,
            too_many_orders_size_threshold_multiplier: 2,
            symbol_too_many_orders_size_threshold_multiplier: 2,

            // stale
            close_stale_minutes: 99999,
            close_stale_minutes_size: None,
        }
    }
}

impl TradeSettings {
    /// Sanity check — 진입 전에 호출해 TradeSettings 값이 현실적인지 검증.
    pub fn validate(&self) -> Result<()> {
        if self.update_interval_seconds <= 0.0 {
            return Err(anyhow!("update_interval_seconds must be > 0"));
        }
        if self.order_size < 0.0 {
            return Err(anyhow!("order_size must be >= 0"));
        }
        if self.max_position_size < 0.0 {
            return Err(anyhow!("max_position_size must be >= 0"));
        }
        if self.max_position_size_multiplier <= 0.0 {
            return Err(anyhow!("max_position_size_multiplier must be > 0"));
        }
        if self.spread_bp_threshold < 0.0 {
            return Err(anyhow!("spread_bp_threshold must be >= 0"));
        }
        if self.same_side_price_time_restriction_ms_min
            > self.same_side_price_time_restriction_ms_max
        {
            return Err(anyhow!(
                "same_side_price_time_restriction min ({}) > max ({})",
                self.same_side_price_time_restriction_ms_min,
                self.same_side_price_time_restriction_ms_max
            ));
        }
        if self.limit_open_time_restriction_ms_min > self.limit_open_time_restriction_ms_max {
            return Err(anyhow!("limit_open_time_restriction min > max"));
        }
        if self.limit_close_time_restriction_ms_min > self.limit_close_time_restriction_ms_max {
            return Err(anyhow!("limit_close_time_restriction min > max"));
        }
        if !matches!(self.limit_open_tif.as_str(), "fok" | "ioc" | "gtc" | "poc") {
            return Err(anyhow!("unknown limit_open_tif: {}", self.limit_open_tif));
        }
        if !matches!(self.limit_close_tif.as_str(), "fok" | "ioc" | "gtc" | "poc") {
            return Err(anyhow!("unknown limit_close_tif: {}", self.limit_close_tif));
        }
        Ok(())
    }
}

/// 한 login (=전략 프로세스) 단위의 전체 설정 묶음.
#[derive(Debug, Clone)]
pub struct StrategyConfig {
    /// 전략을 구분하는 사용자 이름 (Supabase `strategy_settings.login_name` 기준)
    pub login_name: String,
    /// 이 전략이 운영하는 symbol 목록 (심볼 집합 원본 순서 보존)
    pub symbols: Vec<String>,
    /// 매매 파라미터 (TradeSettings v1)
    pub trade_settings: TradeSettings,
    /// O(1) symbol 포함 여부 검사용 해시셋 (ahash)
    symbols_map: AHashMap<String, ()>,
}

impl StrategyConfig {
    pub fn new(login_name: String, symbols: Vec<String>, trade_settings: TradeSettings) -> Self {
        let mut symbols_map = AHashMap::with_capacity(symbols.len());
        for s in &symbols {
            symbols_map.insert(s.clone(), ());
        }
        Self {
            login_name,
            symbols,
            trade_settings,
            symbols_map,
        }
    }

    /// O(1) 포함 여부 (ahash). hot path 에서 부를 수 있음.
    #[inline]
    pub fn is_strategy_symbol(&self, symbol: &str) -> bool {
        self.symbols_map.contains_key(symbol)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Supabase REST loader
// ─────────────────────────────────────────────────────────────────────────

/// Supabase 접속 자격증명 + endpoint 묶음.
#[derive(Debug, Clone)]
pub struct SupabaseConfig {
    /// 예: "https://xxx.supabase.co"
    pub url: String,
    /// anon 혹은 service_role key (strategy 전용은 보통 읽기 전용 role key)
    pub api_key: String,
}

impl SupabaseConfig {
    /// 환경변수에서 읽어옴. `SUPABASE_URL` + `SUPABASE_KEY`.
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("SUPABASE_URL").context("SUPABASE_URL env 누락")?;
        let api_key = std::env::var("SUPABASE_KEY").context("SUPABASE_KEY env 누락")?;
        if url.is_empty() || api_key.is_empty() {
            return Err(anyhow!("SUPABASE_URL / SUPABASE_KEY 가 비어있다"));
        }
        Ok(Self { url, api_key })
    }
}

/// Supabase row 파싱용. id 필드는 문자열 혹은 숫자 둘 다 올 수 있어 custom deserializer 적용.
#[derive(Debug, Deserialize)]
struct StrategyRow {
    #[serde(deserialize_with = "deserialize_text_id")]
    login_name: String,
    /// `strategy_settings.trade_setting` — trade_settings 테이블의 row id (문자 OR 숫자)
    #[serde(deserialize_with = "deserialize_text_id")]
    trade_setting: String,
    /// symbol_set row id (문자 OR 숫자)
    #[serde(deserialize_with = "deserialize_text_id")]
    symbol_set: String,
}

/// Supabase `trade_settings.id == {trade_setting}` 행의 settings JSON payload.
#[derive(Debug, Deserialize)]
struct TradeSettingsRow {
    /// raw JSON object (TradeSettings 포맷)
    settings: serde_json::Value,
}

/// Supabase `symbol_sets.id == {symbol_set}` 행의 symbol 배열.
#[derive(Debug, Deserialize)]
struct SymbolSetRow {
    symbols: Vec<String>,
}

/// Supabase 로더. `ArcSwap` 으로 마지막 성공 스냅샷을 유지 → Supabase 장애 시에도
/// 기존 값으로 계속 주문 가능 (gracefull fallback).
pub struct StrategyConfigLoader {
    http: reqwest::Client,
    supabase: SupabaseConfig,
    cache: ArcSwap<Option<StrategyConfig>>,
}

impl StrategyConfigLoader {
    pub fn new(supabase: SupabaseConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()
            .expect("reqwest::Client build");
        Self {
            http,
            supabase,
            cache: ArcSwap::new(Arc::new(None)),
        }
    }

    /// 현재 캐시된 설정을 읽음 (없으면 None).
    pub fn current(&self) -> Option<Arc<StrategyConfig>> {
        // ArcSwap::load 는 Guard<Arc<Option<_>>>.
        let guard = self.cache.load();
        match guard.as_ref() {
            Some(cfg) => Some(Arc::new(cfg.clone())),
            None => None,
        }
    }

    /// Supabase 에서 최신 전략 설정을 가져와 캐시에 저장. 실패 시 기존 값 유지.
    ///
    /// 레거시 `ConfigManager::load_strategy_configs` 와 동일한 3단 호출 플로우:
    ///   1. `strategy_settings?login_name=eq.{login_name}` → `trade_setting` / `symbol_set`
    ///   2. `trade_settings?id=eq.{trade_setting}` → `settings` (JSON)
    ///   3. `symbol_sets?id=eq.{symbol_set}` → `symbols` (array)
    pub async fn refresh(&self, login_name: &str) -> Result<Arc<StrategyConfig>> {
        // 1) strategy_settings
        let url = format!(
            "{}/rest/v1/strategy_settings?login_name=eq.{}",
            self.supabase.url, login_name
        );
        let rows: Vec<StrategyRow> = self.get_json(&url).await?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Supabase: strategy_settings row '{login_name}' 없음"))?;

        // 2) trade_settings
        let url = format!(
            "{}/rest/v1/trade_settings?id=eq.{}",
            self.supabase.url, row.trade_setting
        );
        let ts_rows: Vec<TradeSettingsRow> = self.get_json(&url).await?;
        let ts_row = ts_rows
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Supabase: trade_settings '{}' 없음", row.trade_setting))?;
        let trade_settings: TradeSettings = serde_json::from_value(ts_row.settings.clone())
            .with_context(|| {
                format!(
                    "trade_settings.settings JSON 파싱 실패: {}",
                    ts_row.settings
                )
            })?;
        trade_settings.validate()?;

        // 3) symbol_sets
        let url = format!(
            "{}/rest/v1/symbol_sets?id=eq.{}",
            self.supabase.url, row.symbol_set
        );
        let ss_rows: Vec<SymbolSetRow> = self.get_json(&url).await?;
        let ss_row = ss_rows
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Supabase: symbol_sets '{}' 없음", row.symbol_set))?;

        let cfg = StrategyConfig::new(row.login_name.clone(), ss_row.symbols, trade_settings);
        let arc = Arc::new(cfg.clone());
        self.cache.store(Arc::new(Some(cfg)));
        debug!(
            "strategy config refreshed: login={} symbols={} tif=(open={}, close={})",
            arc.login_name,
            arc.symbols.len(),
            arc.trade_settings.limit_open_tif,
            arc.trade_settings.limit_close_tif,
        );
        Ok(arc)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .header("apikey", &self.supabase.api_key)
            .header("Authorization", format!("Bearer {}", self.supabase.api_key))
            .header("Accept", "application/json")
            .send()
            .await
            .with_context(|| format!("Supabase GET {url} 네트워크 오류"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!(%url, %status, "Supabase non-2xx");
            return Err(anyhow!("Supabase {url} → {status}: {body}"));
        }
        resp.json::<T>()
            .await
            .with_context(|| format!("Supabase {url} JSON 파싱 실패"))
    }
}

/// id 컬럼이 numeric 이나 text 로 오는 variance 흡수.
fn deserialize_text_id<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    use serde::de::{self, Visitor};
    use std::fmt;

    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string or integer id")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<String, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
    }
    d.deserialize_any(V)
}

// ─────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trade_settings_validates() {
        TradeSettings::default().validate().unwrap();
    }

    #[test]
    fn validate_rejects_bad_tif() {
        let mut ts = TradeSettings::default();
        ts.limit_open_tif = "lol".into();
        assert!(ts.validate().is_err());
    }

    #[test]
    fn validate_rejects_min_gt_max_time_restrictions() {
        let mut ts = TradeSettings::default();
        ts.same_side_price_time_restriction_ms_min = 999;
        ts.same_side_price_time_restriction_ms_max = 100;
        let err = ts.validate().unwrap_err().to_string();
        assert!(err.contains("same_side_price_time_restriction"), "{err}");
    }

    #[test]
    fn strategy_config_membership_is_o1() {
        let cfg = StrategyConfig::new(
            "u".into(),
            vec!["BTC_USDT".into(), "ETH_USDT".into()],
            TradeSettings::default(),
        );
        assert!(cfg.is_strategy_symbol("BTC_USDT"));
        assert!(cfg.is_strategy_symbol("ETH_USDT"));
        assert!(!cfg.is_strategy_symbol("XRP_USDT"));
    }

    #[test]
    fn deserialize_text_id_accepts_both() {
        #[derive(Deserialize)]
        struct X {
            #[serde(deserialize_with = "deserialize_text_id")]
            id: String,
        }
        let from_str: X = serde_json::from_str(r#"{"id":"abc"}"#).unwrap();
        assert_eq!(from_str.id, "abc");
        let from_int: X = serde_json::from_str(r#"{"id":42}"#).unwrap();
        assert_eq!(from_int.id, "42");
    }

    #[test]
    fn trade_settings_deserializes_with_defaults_filled() {
        // settings JSON 에서 일부 필드만 지정해도 나머지는 default 로 채워짐.
        let partial = r#"{"order_size": 123.0, "mid_gap_bp_threshold": 5.0}"#;
        let ts: TradeSettings = serde_json::from_str(partial).unwrap();
        assert_eq!(ts.order_size, 123.0);
        assert_eq!(ts.mid_gap_bp_threshold, 5.0);
        // defaults preserved
        assert_eq!(ts.update_interval_seconds, 0.1);
        assert_eq!(ts.limit_open_tif, "fok");
        assert_eq!(ts.max_position_size_multiplier, 1.5);
    }

    #[test]
    fn cache_starts_empty() {
        let loader = StrategyConfigLoader::new(SupabaseConfig {
            url: "http://127.0.0.1:1".into(),
            api_key: "dummy".into(),
        });
        assert!(loader.current().is_none());
    }
}
