//! hft-exchange-rest — 거래소 REST API 공통 인프라.
//!
//! ## 제공
//! 1. **Credentials** — api_key / api_secret / (옵션) passphrase 캡슐화.
//! 2. **HMAC 서명 helper** — SHA256/SHA512 × hex/base64 조합.
//! 3. **RestClient** — reqwest 기반 얇은 래퍼 (timeout, retry, UA).
//! 4. **에러 매핑** — HTTP status/body → [`ApiError`].
//!
//! ## 비의도적 범위 밖
//! - 거래소별 payload/쿼리 조립 → 각 executor 구현체.
//! - WebSocket 주문 스트림 → 각 거래소 crate.
//!
//! ## 설계 원칙
//! - **메모리 최소화가 아닌 올바름 우선**: cold path (주문 1건 당 수 ms) 이므로
//!   hot-path allocator trick 불필요. 그러나 secret 재할당은 피한다
//!   (`Credentials` 는 `Arc<String>` 공유 보관 가능).
//! - **테스트 가능성**: 모든 서명 함수는 RFC 4231 / 거래소 공식 샘플 기반
//!   단위 테스트로 검증. `RestClient` 는 `reqwest::Client` 를 인젝션 받을 수
//!   있어 mock 서버와 통합 테스트 가능.
//! - **재시도 정책**: 5xx 는 총 3회 (backoff 100/300/900 ms) 재시도,
//!   4xx 는 즉시 반환. 429 는 `Retry-After` 헤더 존중 + `ApiError::RateLimited`.
//! - **로깅**: error 레벨은 최종 실패시만. retry 는 debug.
//!
//! ## 보안
//! - secret 은 절대 로그/Display 에 찍지 않는다 (`Credentials::Debug` 는 masking).
//! - URL 쿼리에 secret 이 들어갈 수 있는 거래소는 없다 (전부 header 기반).

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hft_exchange_api::ApiError;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use sha2::{Sha256, Sha512};
use tracing::{debug, warn};

// ────────────────────────────────────────────────────────────────────────────
// 자격 증명
// ────────────────────────────────────────────────────────────────────────────

/// API 자격 증명 — secret 은 외부에 노출되지 않도록 `Debug` 구현에서 masking.
///
/// `Arc` 로 감싸면 clone 이 cheap 해지므로 여러 executor 가 공유해도 부담 없다.
#[derive(Clone)]
pub struct Credentials {
    pub api_key: Arc<str>,
    pub api_secret: Arc<str>,
    /// OKX / Bitget 이 요구. 다른 거래소는 `None`.
    pub passphrase: Option<Arc<str>>,
}

impl Credentials {
    pub fn new(api_key: impl Into<Arc<str>>, api_secret: impl Into<Arc<str>>) -> Self {
        Self {
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            passphrase: None,
        }
    }

    pub fn with_passphrase(mut self, passphrase: impl Into<Arc<str>>) -> Self {
        self.passphrase = Some(passphrase.into());
        self
    }

    /// 이 구조체의 자격이 비어있지 않은지 확인.
    ///
    /// 모든 field 가 ASCII-trim 후 비어있으면 `Err(ApiError::Auth)`.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.api_key.trim().is_empty() {
            return Err(ApiError::Auth("api_key is empty".into()));
        }
        if self.api_secret.trim().is_empty() {
            return Err(ApiError::Auth("api_secret is empty".into()));
        }
        Ok(())
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &mask(&self.api_key))
            .field("api_secret", &"***")
            .field(
                "passphrase",
                &if self.passphrase.is_some() { "<set>" } else { "<none>" },
            )
            .finish()
    }
}

fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4 {
        "****".to_string()
    } else {
        let head: String = s.chars().take(2).collect();
        let tail: String = s.chars().skip(n - 2).collect();
        format!("{head}***{tail}")
    }
}

// ────────────────────────────────────────────────────────────────────────────
// HMAC 서명 helpers
// ────────────────────────────────────────────────────────────────────────────

type HmacSha256 = Hmac<Sha256>;
type HmacSha512 = Hmac<Sha512>;

/// HMAC-SHA256 → hex (lowercase).
///
/// secret 이 빈 문자열이어도 panic 하지 않는다 (HMAC 는 빈 key 허용).
pub fn hmac_sha256_hex(secret: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret)
        .expect("HMAC can take key of any size");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

/// HMAC-SHA256 → base64 (standard, padded).
pub fn hmac_sha256_base64(secret: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(msg);
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// HMAC-SHA512 → hex (lowercase). Gate.io 서명에 사용.
pub fn hmac_sha512_hex(secret: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha512::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

/// SHA-512 hex digest — Gate.io 는 body 를 먼저 SHA-512(hex) 로 해시하고 그 hex
/// 문자열을 message 에 포함시킨다.
pub fn sha512_hex(msg: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = Sha512::new();
    hasher.update(msg);
    hex::encode(hasher.finalize())
}

// ────────────────────────────────────────────────────────────────────────────
// 에러 매핑
// ────────────────────────────────────────────────────────────────────────────

/// HTTP status + body → `ApiError`.
///
/// - 401/403 → Auth
/// - 429 → RateLimited (`Retry-After` 초 단위 → ms)
/// - 4xx → Rejected(body)
/// - 5xx → Connection(body) — 호출자가 retry 판단
pub fn map_http_error(status: StatusCode, retry_after_ms: Option<u64>, body: &str) -> ApiError {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return ApiError::Auth(format!("http {}: {}", status.as_u16(), truncate(body, 512)));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ApiError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(1_000),
        };
    }
    if status.is_client_error() {
        return ApiError::Rejected(format!("http {}: {}", status.as_u16(), truncate(body, 512)));
    }
    ApiError::Connection(format!("http {}: {}", status.as_u16(), truncate(body, 512)))
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // UTF-8 안전 truncate (char boundary 까지 뒤로 밀기).
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?;
    let s = v.to_str().ok()?.trim();
    // 숫자면 초 단위, RFC 1123 date 면 대충 무시하고 1s.
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs.saturating_mul(1_000));
    }
    Some(1_000)
}

// ────────────────────────────────────────────────────────────────────────────
// RestClient
// ────────────────────────────────────────────────────────────────────────────

/// 응답 요약 — executor 가 status 와 body 를 모두 검사해야 하는 경우가 많아
/// reqwest::Response 가 아니라 materialize 된 (status, body) 를 돌려준다.
#[derive(Debug)]
pub struct RestResponse {
    pub status: StatusCode,
    pub body: String,
    pub headers: HeaderMap,
}

impl RestResponse {
    /// 성공(2xx) 이면 body 를 반환, 아니면 `ApiError` 로 변환해 `Err`.
    pub fn into_ok_body(self) -> Result<String, ApiError> {
        if self.status.is_success() {
            Ok(self.body)
        } else {
            Err(map_http_error(
                self.status,
                parse_retry_after(&self.headers),
                &self.body,
            ))
        }
    }
}

/// 재시도 정책.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 최대 시도 횟수 (초기 시도 포함). 1 이면 재시도 없음.
    pub max_attempts: u32,
    /// 재시도 간격 base (exponential × attempt).
    pub base_backoff_ms: u64,
    /// 상한.
    pub max_backoff_ms: u64,
}

impl RetryPolicy {
    pub const fn none() -> Self {
        Self { max_attempts: 1, base_backoff_ms: 0, max_backoff_ms: 0 }
    }
    pub const fn defaults() -> Self {
        Self { max_attempts: 3, base_backoff_ms: 100, max_backoff_ms: 1_000 }
    }
    fn delay(&self, attempt: u32) -> Duration {
        let shift = attempt.min(10);
        let d = self.base_backoff_ms.saturating_mul(1u64 << shift);
        Duration::from_millis(d.min(self.max_backoff_ms))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::defaults()
    }
}

/// 얇은 reqwest 래퍼 — 공통 timeout / UA / retry 를 제공.
#[derive(Debug, Clone)]
pub struct RestClient {
    inner: reqwest::Client,
    user_agent: String,
    retry: RetryPolicy,
}

impl RestClient {
    /// 기본 생성: 5s timeout, `User-Agent: hft-monorepo/0.1`, default retry.
    pub fn new() -> Result<Self, ApiError> {
        let inner = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_nodelay(true)
            // redirect 는 금지 — 거래소 API 는 redirect 할 일 없음, 보안상 안전.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ApiError::Connection(format!("reqwest build: {e}")))?;
        Ok(Self {
            inner,
            user_agent: "hft-monorepo/0.1".to_string(),
            retry: RetryPolicy::defaults(),
        })
    }

    /// 사전 구성된 client 를 주입 (테스트 / 특수 TLS 설정용).
    pub fn with_client(inner: reqwest::Client) -> Self {
        Self {
            inner,
            user_agent: "hft-monorepo/0.1".to_string(),
            retry: RetryPolicy::defaults(),
        }
    }

    pub fn set_user_agent(&mut self, ua: impl Into<String>) -> &mut Self {
        self.user_agent = ua.into();
        self
    }

    pub fn set_retry(&mut self, retry: RetryPolicy) -> &mut Self {
        self.retry = retry;
        self
    }

    /// 빌더에 공통 header (UA) 적용.
    fn build_request(
        &self,
        method: Method,
        url: &str,
        headers: &HeaderMap,
        body: Option<String>,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .inner
            .request(method, url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json");
        for (k, v) in headers.iter() {
            req = req.header(k, v);
        }
        if let Some(b) = body {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(b);
        }
        req
    }

    /// 실제 전송 + 재시도. 5xx / connection error 만 재시도, 4xx 는 즉시 반환.
    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &HeaderMap,
        body: Option<String>,
    ) -> Result<RestResponse, ApiError> {
        let mut last_err: Option<ApiError> = None;
        let max = self.retry.max_attempts.max(1);

        for attempt in 0..max {
            if attempt > 0 {
                let delay = self.retry.delay(attempt - 1);
                debug!(
                    target: "rest",
                    attempt = attempt,
                    delay_ms = delay.as_millis() as u64,
                    url = %url,
                    "retrying"
                );
                tokio::time::sleep(delay).await;
            }

            let req = self.build_request(method.clone(), url, headers, body.clone());
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    // connection-level error 는 재시도 대상.
                    let mapped = ApiError::Connection(format!("send: {e}"));
                    warn!(target: "rest", attempt = attempt, error = %e, "request send failed");
                    last_err = Some(mapped);
                    continue;
                }
            };

            let status = resp.status();
            let headers_out = resp.headers().clone();
            // body 는 항상 읽어와야 connection 재사용에 유리.
            let body_out = match resp.text().await {
                Ok(s) => s,
                Err(e) => {
                    last_err = Some(ApiError::Decode(format!("body read: {e}")));
                    continue;
                }
            };

            if status.is_success() {
                return Ok(RestResponse {
                    status,
                    body: body_out,
                    headers: headers_out,
                });
            }

            // 5xx — 재시도
            if status.is_server_error() {
                last_err = Some(map_http_error(
                    status,
                    parse_retry_after(&headers_out),
                    &body_out,
                ));
                continue;
            }

            // 4xx (rate limit 포함) — 즉시 반환 (rate limit retry 는 호출자가
            // `ApiError::RateLimited` 를 보고 결정).
            return Ok(RestResponse {
                status,
                body: body_out,
                headers: headers_out,
            });
        }

        Err(last_err.unwrap_or_else(|| ApiError::Connection("unknown".into())))
    }
}

impl Default for RestClient {
    fn default() -> Self {
        Self::new().expect("default RestClient build")
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Header 조립 helper
// ────────────────────────────────────────────────────────────────────────────

/// 문자열 key/value 페어를 `HeaderMap` 으로 안전 변환.
///
/// header value 변환이 실패하면 `ApiError::Decode` 로 변환 — 거래소가 요구하는
/// 특정 문자만 들어가도록 호출자가 사전 sanitize 했다는 가정.
pub fn headers_from_pairs<I, K, V>(pairs: I) -> Result<HeaderMap, ApiError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        let name = HeaderName::from_bytes(k.as_ref().as_bytes())
            .map_err(|e| ApiError::Decode(format!("header name '{}': {e}", k.as_ref())))?;
        let value = HeaderValue::from_str(v.as_ref())
            .map_err(|e| ApiError::Decode(format!("header value for '{}': {e}", k.as_ref())))?;
        map.insert(name, value);
    }
    Ok(map)
}

// ────────────────────────────────────────────────────────────────────────────
// 현재 wall-clock helper — 거래소 대부분 요구.
// ────────────────────────────────────────────────────────────────────────────

/// 현재 epoch ms. chrono 없이 std 만 사용.
pub fn now_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        // 시스템 clock 이 1970 이전일 경우 — 사실상 발생하지 않음.
        Err(_) => 0,
    }
}

/// 현재 epoch seconds (Gate/Binance 일부 endpoint 가 초 단위 요구).
pub fn now_epoch_s() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4231 test case 1: key = "0x0b" * 20, data = "Hi There"
    // HMAC-SHA256 expected: b0344c61d8db38535ca8afceaf0bf12b 881dc200c9833da726e9376c2e32cff7
    #[test]
    fn hmac_sha256_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let msg = b"Hi There";
        let hex = hmac_sha256_hex(&key, msg);
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    // RFC 4231 test case 2: key = "Jefe", data = "what do ya want for nothing?"
    // HMAC-SHA256 expected: 5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843
    #[test]
    fn hmac_sha256_rfc4231_case2() {
        let key = b"Jefe";
        let msg = b"what do ya want for nothing?";
        assert_eq!(
            hmac_sha256_hex(key, msg),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    // RFC 4231 test case 2 → HMAC-SHA512 expected
    #[test]
    fn hmac_sha512_rfc4231_case2() {
        let key = b"Jefe";
        let msg = b"what do ya want for nothing?";
        assert_eq!(
            hmac_sha512_hex(key, msg),
            "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea2505549758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
        );
    }

    #[test]
    fn hmac_sha256_base64_matches_hex_roundtrip() {
        let key = b"secret";
        let msg = b"message";
        let h = hmac_sha256_hex(key, msg);
        let b = hmac_sha256_base64(key, msg);
        // decode base64 → compare with hex bytes
        let decoded = base64::engine::general_purpose::STANDARD.decode(b).unwrap();
        assert_eq!(hex::encode(decoded), h);
    }

    #[test]
    fn sha512_hex_known_vector() {
        // echo -n "" | sha512sum
        let empty = sha512_hex(b"");
        assert_eq!(
            empty,
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        // echo -n "abc"
        let abc = sha512_hex(b"abc");
        assert_eq!(
            abc,
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn credentials_validate_rejects_empty() {
        let c = Credentials::new("", "s");
        assert!(c.validate().is_err());
        let c = Credentials::new("k", "   ");
        assert!(c.validate().is_err());
        let c = Credentials::new("k", "s");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn credentials_debug_masks_secret() {
        let c = Credentials::new("mykeylong", "topsecret").with_passphrase("pass");
        let s = format!("{:?}", c);
        assert!(!s.contains("topsecret"), "secret must be masked: {s}");
        assert!(s.contains("***"));
    }

    #[test]
    fn map_http_error_buckets() {
        use reqwest::StatusCode as S;
        match map_http_error(S::UNAUTHORIZED, None, "bad key") {
            ApiError::Auth(_) => {}
            other => panic!("expected Auth, got {:?}", other),
        }
        match map_http_error(S::TOO_MANY_REQUESTS, Some(500), "slow") {
            ApiError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 500),
            other => panic!("expected RateLimited, got {:?}", other),
        }
        match map_http_error(S::BAD_REQUEST, None, "invalid") {
            ApiError::Rejected(m) => assert!(m.contains("invalid")),
            other => panic!("expected Rejected, got {:?}", other),
        }
        match map_http_error(S::INTERNAL_SERVER_ERROR, None, "oops") {
            ApiError::Connection(_) => {}
            other => panic!("expected Connection, got {:?}", other),
        }
    }

    #[test]
    fn truncate_is_utf8_safe() {
        // 한글(3 bytes each) 문자열 — UTF-8 경계에서 자르면 panic 위험을 차단.
        let s = "가나다라마바사"; // 7 × 3 = 21 bytes
        let t = truncate(s, 10);
        assert!(!t.is_empty());
        assert!(t.len() <= 10);
        // valid UTF-8
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
    }

    #[test]
    fn retry_policy_delay_saturates() {
        let p = RetryPolicy { max_attempts: 10, base_backoff_ms: 100, max_backoff_ms: 500 };
        // attempt 0: 100, 1: 200, 2: 400, 3: 500(capped), ...
        assert_eq!(p.delay(0), Duration::from_millis(100));
        assert_eq!(p.delay(1), Duration::from_millis(200));
        assert_eq!(p.delay(2), Duration::from_millis(400));
        assert_eq!(p.delay(3), Duration::from_millis(500));
        assert_eq!(p.delay(50), Duration::from_millis(500));
    }

    #[test]
    fn headers_from_pairs_builds_map() {
        let m = headers_from_pairs([("X-KEY", "abc"), ("X-SIG", "def")]).unwrap();
        assert_eq!(m.get("x-key").unwrap().to_str().unwrap(), "abc");
        assert_eq!(m.get("X-SIG").unwrap().to_str().unwrap(), "def");
    }

    #[test]
    fn headers_from_pairs_rejects_invalid_name() {
        let r = headers_from_pairs([("bad header", "v")]);
        assert!(r.is_err());
    }

    #[test]
    fn now_epoch_is_positive() {
        assert!(now_epoch_ms() > 1_700_000_000_000);
        assert!(now_epoch_s() > 1_700_000_000);
    }

    #[test]
    fn mask_short_and_long() {
        assert_eq!(mask(""), "****");
        assert_eq!(mask("abc"), "****");
        assert_eq!(mask("abcd"), "****");
        let m = mask("abcdefgh");
        assert!(m.starts_with("ab"));
        assert!(m.ends_with("gh"));
        assert!(m.contains("***"));
    }
}
