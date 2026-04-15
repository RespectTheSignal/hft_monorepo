//! hft-exchange-common — 거래소 구현체가 공유하는 hot-path primitive.
//!
//! # 설계
//!
//! - 각 거래소 crate (gate/binance/...) 는 자신의 WS schema 를 typed
//!   `#[derive(Deserialize)]` 로 표현하고, 공통 helper (symbol intern, lenient
//!   f64 디코더) 는 이 crate 에서 가져온다.
//! - hot path 의 per-event allocation 을 0 에 가깝게 끌어내는 게 목표.
//!     * `SymbolCache::intern(&str)` — 첫 관측 시 1회 `Arc<str>` alloc,
//!       그 이후엔 `Arc::clone` (refcount 증가, heap 접근 없음).
//!     * `JsonScratch` — WS text 를 mutable `Vec<u8>` 로 reusable 하게 복사해
//!       `serde_json::from_slice` / `simd-json::to_borrowed_value` 등에 전달.
//!     * `deserialize_f64_lenient` — serde visitor 로 str/number 를 모두 f64 로
//!       수용. 문자열을 parse 할 때만 일시적인 stack 소비.
//! - 거래소별 WS schema 는 절대 이 crate 에 들어오지 않는다. 구현체 경계를
//!   유지해 이 crate 의 API 가 부풀지 않도록 한다.
//!
//! # 성능 고려
//!
//! - `SymbolCache` 는 read-heavy (99% read, 0.001% write) 라 `DashMap` 기반.
//!   내부 shard 수는 기본값을 유지해 ahash + striped lock 이 경합을 최소화한다.
//! - `intern` 은 hot path 에서 호출되므로 `&self` 만 받고 `&mut` 을 피한다.
//! - `JsonScratch::reset_and_fill(&mut self, &str)` 는 `Vec::clear` + `extend_from_slice`
//!   로 capacity 를 재사용 (per-event allocation 없음, capacity 는 WS frame
//!   최대 크기에 근접하면 안정).

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]
#![forbid(unsafe_code)]

use std::fmt;
use std::sync::Arc;

use dashmap::DashMap;
use hft_time::{Clock, LatencyStamps, Stage};
use hft_types::Symbol;

// ────────────────────────────────────────────────────────────────────────────
// SymbolCache — `Arc<str>` intern pool
// ────────────────────────────────────────────────────────────────────────────

/// 심볼 문자열을 공유 `Arc<str>` 로 중복 alloc 없이 해결한다.
///
/// ## Why
/// 거래소 WS 메시지는 **같은 심볼을 초당 수백 번** 보낸다. 매 번 `Symbol::new(s)`
/// 를 호출하면 이벤트당 `Arc<str>` heap alloc 이 들어가 hot path 에서 ~100ns,
/// 그리고 더 중요하게 **캐시 라인 오염** 을 유발한다.
///
/// ## How
/// - 내부 `DashMap<String, Symbol>` 에 문자열을 키로 저장.
/// - `intern(&str)` 은 먼저 read-only lookup. hit 이면 `Arc::clone` (refcount++)
///   으로 반환 — heap 접근 없음.
/// - miss 시에만 key 를 소유 `String` 으로 복사하고 `Symbol::new` 로 새 Arc 생성.
/// - Arc 자체는 immutable 하므로 concurrent reader 가 자유롭게 복사 가능.
///
/// ## Bounded 정책
/// - 바운드가 없다. 거래소 심볼 수는 10^3 수준이라 unbounded 로도 메모리 폭주
///   위험이 없다. 만약 악의적 / 버그성 입력이 들어와도 유니버스가 좁아 수 MB 이내.
/// - 실수로 ticker 단위 (BTC, ETH, ...) 를 넣어도 전체 수백 KB.
///
/// ## Lifetime
/// - `SymbolCache` 자체는 `Arc<SymbolCache>` 로 감싸 여러 스레드/task 가 공유한다.
///   각 거래소 feed 는 자신의 생성자에서 주입된 `Arc<SymbolCache>` 를 보관.
#[derive(Default)]
pub struct SymbolCache {
    /// ahash 기반 concurrent map — fast non-crypto hasher.
    inner: DashMap<String, Symbol, ahash::RandomState>,
}

impl SymbolCache {
    /// 새 빈 캐시.
    pub fn new() -> Self {
        Self {
            inner: DashMap::with_hasher(ahash::RandomState::new()),
        }
    }

    /// 미리 알려진 심볼을 채운다 — warm-up 시 "첫 frame 의 intern 비용" 을
    /// 사전에 지불해 hot path 지터를 줄인다.
    pub fn prewarm<I, S>(&self, symbols: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for s in symbols {
            let _ = self.intern(s.as_ref());
        }
    }

    /// 심볼 문자열을 intern. 이미 존재하면 `Arc::clone` 으로 반환.
    ///
    /// hot path 호출 — `&self` 만 받고 내부 동기화는 shard lock 으로 최소화.
    #[inline]
    pub fn intern(&self, s: &str) -> Symbol {
        if let Some(found) = self.inner.get(s) {
            return found.value().clone();
        }
        // miss: 새 심볼을 삽입. 중간에 다른 스레드가 먼저 삽입했을 수 있으므로
        // entry API 로 race-safe insert.
        self.inner
            .entry(s.to_owned())
            .or_insert_with(|| Symbol::new(s))
            .value()
            .clone()
    }

    /// 현재 intern 된 심볼 수 (디버그/메트릭).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 비었는지.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl fmt::Debug for SymbolCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SymbolCache")
            .field("len", &self.len())
            .finish()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// JsonScratch — reusable parse buffer
// ────────────────────────────────────────────────────────────────────────────

/// WS text frame 을 파싱하기 위한 reusable byte 버퍼.
///
/// ## Why
/// - `serde_json::from_str(&text)` 는 내부적으로 UTF-8 validation 후 byte slice 를
///   쓴다. 이미 `&str` 이면 validation 은 빠르지만, 이 struct 는 **simd-json /
///   in-place JSON mutator** 등 `&mut [u8]` 을 요구하는 파서와도 호환되기 위해
///   따로 버퍼를 둔다.
/// - 또, 디버그 목적으로 원본 텍스트를 짧게 저장해두는 길이가 일정하므로
///   (`Vec::clear` + `extend_from_slice`) capacity 재사용이 좋다.
///
/// ## 사용 패턴
/// ```ignore
/// let mut scratch = JsonScratch::with_capacity(4096);
/// while let Some(msg) = ws.next().await {
///     if let Message::Text(text) = msg {
///         let bytes = scratch.reset_and_fill(&text);
///         let frame: GateFrame<'_> = serde_json::from_slice(bytes)?;
///         // ... emit ...
///     }
/// }
/// ```
///
/// ## 주의
/// - `bytes` 의 lifetime 은 `&mut self` 에 묶이므로, parse 결과가 buffer 를
///   borrow 하는 한 (`#[serde(borrow)]` 구조체) `scratch` 를 재사용할 수 없다.
///   즉 "parse → emit → 다음 frame" 까지는 `frame` 이 drop 된 이후.
pub struct JsonScratch {
    buf: Vec<u8>,
}

impl JsonScratch {
    /// 기본 용량으로 생성 (4096 bytes — 일반 WS frame 커버).
    pub fn new() -> Self {
        Self::with_capacity(4096)
    }

    /// 주어진 capacity 로 생성.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// 버퍼를 비우고 `text` 를 바이트 복사. capacity 가 부족하면 grow.
    /// 반환되는 `&mut [u8]` 는 파서가 in-place mutate 해도 됨.
    #[inline]
    pub fn reset_and_fill(&mut self, text: &str) -> &mut [u8] {
        self.buf.clear();
        self.buf.extend_from_slice(text.as_bytes());
        &mut self.buf
    }

    /// 비어있는 `&mut Vec<u8>` 반환 — 호출자가 직접 write.
    pub fn reset(&mut self) -> &mut Vec<u8> {
        self.buf.clear();
        &mut self.buf
    }

    /// 현재 capacity (진단용).
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }
}

impl Default for JsonScratch {
    fn default() -> Self {
        Self::new()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// serde helper: f64_lenient (string 또는 number 허용)
// ────────────────────────────────────────────────────────────────────────────

/// 거래소 JSON 은 가격/수량 필드를 `"100.5"` (string) 로도, `100.5` (number) 로도
/// 보낸다. 한쪽만 받으면 schema drift 발생 시 잠복 버그로 이어진다.
///
/// 이 함수를 `#[serde(deserialize_with = "deserialize_f64_lenient")]` 로 붙이면
/// `Option<f64>` / `f64` 필드에 둘 다 파싱된다.
///
/// ## 사용
/// ```ignore
/// #[derive(Deserialize)]
/// struct Level<'a> {
///     #[serde(borrow)]
///     p: &'a str,
///     #[serde(deserialize_with = "deserialize_f64_lenient")]
///     size: f64,
/// }
/// ```
///
/// ## 예외
/// - `null` / 누락 필드는 이 함수로 처리하지 않는다. `Option<f64>` 에 결합해서
///   `#[serde(default, deserialize_with = "...")]` 패턴을 쓸 것 —
///   단, Option 에 붙이려면 [`deserialize_f64_lenient_opt`] 를 써야 한다.
pub fn deserialize_f64_lenient<'de, D>(d: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = f64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a number or a string containing a number")
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<f64, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<f64, E> {
            v.trim()
                .parse::<f64>()
                .map_err(|_| de::Error::custom(format!("invalid f64 string: {v:?}")))
        }
        fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<f64, E> {
            self.visit_str(v)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<f64, E> {
            self.visit_str(&v)
        }
    }
    d.deserialize_any(V)
}

/// `Option<f64>` 용 lenient 디코더 — `null`/누락 → `None`.
pub fn deserialize_f64_lenient_opt<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<f64>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a number, numeric string, or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Option<f64>, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Option<f64>, E> {
            Ok(None)
        }
        fn visit_some<D2>(self, d: D2) -> Result<Option<f64>, D2::Error>
        where
            D2: serde::Deserializer<'de>,
        {
            deserialize_f64_lenient(d).map(Some)
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Option<f64>, E> {
            Ok(Some(v))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<f64>, E> {
            Ok(Some(v as f64))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<f64>, E> {
            Ok(Some(v as f64))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<f64>, E> {
            if v.is_empty() || v.eq_ignore_ascii_case("null") {
                return Ok(None);
            }
            v.trim()
                .parse::<f64>()
                .map(Some)
                .map_err(|_| de::Error::custom(format!("invalid f64 string: {v:?}")))
        }
        fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Option<f64>, E> {
            self.visit_str(v)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<Option<f64>, E> {
            self.visit_str(&v)
        }
    }
    d.deserialize_any(V)
}

// ────────────────────────────────────────────────────────────────────────────
// serde helper: i64_lenient (string 또는 number 허용)
// ────────────────────────────────────────────────────────────────────────────

/// `trade_id` / `timestamp` 같이 거래소가 string/number 를 혼용하는 정수 필드용.
pub fn deserialize_i64_lenient<'de, D>(d: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = i64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an integer or numeric string")
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            v.trim()
                .parse::<i64>()
                .map_err(|_| de::Error::custom(format!("invalid i64 string: {v:?}")))
        }
        fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<i64, E> {
            self.visit_str(v)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<i64, E> {
            self.visit_str(&v)
        }
    }
    d.deserialize_any(V)
}

/// `Option<i64>` lenient — null/누락 허용.
pub fn deserialize_i64_lenient_opt<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<i64>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an integer, numeric string, or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Option<i64>, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Option<i64>, E> {
            Ok(None)
        }
        fn visit_some<D2>(self, d: D2) -> Result<Option<i64>, D2::Error>
        where
            D2: serde::Deserializer<'de>,
        {
            deserialize_i64_lenient(d).map(Some)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<i64>, E> {
            Ok(Some(v))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<i64>, E> {
            Ok(Some(v as i64))
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Option<i64>, E> {
            Ok(Some(v as i64))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<i64>, E> {
            if v.is_empty() || v.eq_ignore_ascii_case("null") {
                return Ok(None);
            }
            v.trim()
                .parse::<i64>()
                .map(Some)
                .map_err(|_| de::Error::custom(format!("invalid i64 string: {v:?}")))
        }
        fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Option<i64>, E> {
            self.visit_str(v)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<Option<i64>, E> {
            self.visit_str(&v)
        }
    }
    d.deserialize_any(V)
}

// ────────────────────────────────────────────────────────────────────────────
// LatencyStamps 빌더
// ────────────────────────────────────────────────────────────────────────────

/// 거래소 feed 가 표준적으로 채우는 stamp 쌍 생성:
/// `ws_received` = 지금, `exchange_server_ms` = 프레임의 server time.
///
/// 거래소 구현체 코드에서 반복되는 패턴이라 공통화.
#[inline]
pub fn build_ws_stamps<C: Clock + ?Sized>(clock: &C, server_time_ms: i64) -> LatencyStamps {
    let mut ls = LatencyStamps::new();
    ls.mark(Stage::WsReceived, clock);
    ls.exchange_server = hft_time::Stamp {
        wall_ms: server_time_ms,
        mono_ns: 0,
    };
    ls
}

/// 이미 찍어둔 `ws_received` stamp 로 `LatencyStamps` 를 조립.
///
/// 한 WS frame 이 **여러 event** 를 담고 있을 때 (e.g. Gate `futures.trades` 는
/// `result: [trade1, trade2, ...]` 배열) frame 수신 시각은 한 번만 찍고
/// 이벤트마다 stamps 를 이 함수로 복제한다. `Stamp::now()` 를 매 이벤트 호출하면
/// monotonic clock 이 event 간 미세 격차로 나오는데, 이는 "프레임 전체가 같은
/// 시각에 수신됐다" 는 거래소 의도와 불일치하므로 single stamp 재사용이 맞다.
#[inline]
pub fn stamps_with_ws(ws_received: hft_time::Stamp, server_time_ms: i64) -> LatencyStamps {
    let mut ls = LatencyStamps::new();
    ls.ws_received = ws_received;
    ls.exchange_server = hft_time::Stamp {
        wall_ms: server_time_ms,
        mono_ns: 0,
    };
    ls
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn symbol_cache_interns_first_and_clones_rest() {
        let cache = SymbolCache::new();
        let a = cache.intern("BTC_USDT");
        let b = cache.intern("BTC_USDT");
        let c = cache.intern("ETH_USDT");

        // 같은 입력이면 같은 Arc pointer.
        assert!(a.ptr_eq(&b), "same key must return the same Arc");
        // 다른 입력이면 다른 Arc.
        assert!(!a.ptr_eq(&c), "different keys must be different Arcs");

        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn symbol_cache_prewarm_populates() {
        let cache = SymbolCache::new();
        cache.prewarm(["BTC_USDT", "ETH_USDT", "SOL_USDT"]);
        assert_eq!(cache.len(), 3);

        // prewarm 후엔 intern 이 추가 alloc 없이 hit.
        let got = cache.intern("ETH_USDT");
        assert_eq!(got.as_str(), "ETH_USDT");
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn symbol_cache_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SymbolCache>();
        assert_send_sync::<Arc<SymbolCache>>();
    }

    #[test]
    fn json_scratch_reuses_capacity() {
        let mut s = JsonScratch::with_capacity(128);
        let cap0 = s.capacity();

        let _ = s.reset_and_fill("hello world");
        assert!(s.capacity() >= cap0, "capacity must not shrink");

        // 두 번째 호출도 grow 없이 동작해야 — 둘 다 cap 내 크기.
        let cap1 = s.capacity();
        let bytes = s.reset_and_fill("abc");
        assert_eq!(bytes, b"abc");
        assert_eq!(s.capacity(), cap1, "capacity must be reused");
    }

    #[test]
    fn json_scratch_grows_when_needed() {
        let mut s = JsonScratch::with_capacity(4);
        let big = "this is definitely longer than four bytes";
        let bytes = s.reset_and_fill(big);
        assert_eq!(bytes, big.as_bytes());
        assert!(s.capacity() >= big.len());
    }

    #[derive(Deserialize)]
    struct TF {
        #[serde(deserialize_with = "deserialize_f64_lenient")]
        v: f64,
    }

    #[derive(Deserialize)]
    struct TFO {
        #[serde(default, deserialize_with = "deserialize_f64_lenient_opt")]
        v: Option<f64>,
    }

    #[test]
    fn f64_lenient_handles_str_and_number() {
        let a: TF = serde_json::from_str(r#"{"v":"1.5"}"#).unwrap();
        assert!((a.v - 1.5).abs() < 1e-9);
        let b: TF = serde_json::from_str(r#"{"v":2}"#).unwrap();
        assert!((b.v - 2.0).abs() < 1e-9);
        let c: TF = serde_json::from_str(r#"{"v":2.75}"#).unwrap();
        assert!((c.v - 2.75).abs() < 1e-9);
    }

    #[test]
    fn f64_lenient_rejects_garbage() {
        let r: Result<TF, _> = serde_json::from_str(r#"{"v":"not a number"}"#);
        assert!(r.is_err());
    }

    #[test]
    fn f64_lenient_opt_handles_null_and_missing() {
        let a: TFO = serde_json::from_str(r#"{"v":null}"#).unwrap();
        assert_eq!(a.v, None);
        let b: TFO = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(b.v, None);
        let c: TFO = serde_json::from_str(r#"{"v":"3.14"}"#).unwrap();
        assert_eq!(c.v, Some(3.14));
        let d: TFO = serde_json::from_str(r#"{"v":""}"#).unwrap();
        assert_eq!(d.v, None);
    }

    #[derive(Deserialize)]
    struct TI {
        #[serde(deserialize_with = "deserialize_i64_lenient")]
        v: i64,
    }

    #[derive(Deserialize)]
    struct TIO {
        #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
        v: Option<i64>,
    }

    #[test]
    fn i64_lenient_basic() {
        let a: TI = serde_json::from_str(r#"{"v":"42"}"#).unwrap();
        assert_eq!(a.v, 42);
        let b: TI = serde_json::from_str(r#"{"v":-7}"#).unwrap();
        assert_eq!(b.v, -7);
    }

    #[test]
    fn i64_lenient_opt_null() {
        let a: TIO = serde_json::from_str(r#"{"v":null}"#).unwrap();
        assert_eq!(a.v, None);
        let b: TIO = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(b.v, None);
    }

    #[test]
    fn build_stamps_fills_server_time() {
        // MockClock 으로 deterministic 검증.
        let clock = hft_time::MockClock::new(2_000, 12_345);
        let ls = build_ws_stamps(clock.as_ref(), 123_456_789);
        assert_eq!(ls.exchange_server.wall_ms, 123_456_789);
        assert_eq!(ls.exchange_server.mono_ns, 0);
        // ws_received 는 clock 현재 시각.
        assert_eq!(ls.ws_received.wall_ms, 2_000);
        assert_eq!(ls.ws_received.mono_ns, 12_345);
    }
}
