# hft-protocol — SPEC

## 역할
wire format 과 토픽 네이밍. **이 crate 의 바이트 레이아웃은 downstream 의 C/C++ 컨슈머까지 호환**이므로 필드 순서·오프셋은 ADR 수준의 결정.

## 바이트 레이아웃

### BookTicker (120 bytes)
```
offset  size  field
 0      16    symbol  (ASCII, null-padded, 최대 16byte)
16       8    bid_price    (f64 LE)
24       8    ask_price    (f64 LE)
32       8    bid_size     (f64 LE)
40       8    ask_size     (f64 LE)
48       8    event_time_ms  (i64 LE)  ← 거래소 "E"
56       8    server_time_ms (i64 LE)  ← 거래소 "T"
64      56    LatencyStamps (i64 × 7)
120     —     EOF
```

### Trade (128 bytes)
```
offset  size  field
 0      16    symbol
16       8    price
24       8    size
32       8    trade_id (u64 LE)
40       8    create_time_ms
48       8    server_time_ms
56       1    is_buyer_maker (bool → u8)
57       7    padding
64      56    LatencyStamps
120      8    reserved
128     —     EOF
```

(실 gate_hft 와 호환하려면 이 표의 수치는 기존 `serializer.rs` 와 검증 필요.
Phase 1 초기에 **한 번 더 대조**할 것.)

## 공개 API

```rust
// wire.rs
pub const BOOK_TICKER_SIZE: usize = 120;
pub const TRADE_SIZE: usize = 128;
pub const LATENCY_STAMPS_SIZE: usize = 56;

pub fn encode_bookticker(bt: &BookTicker, stamps: &LatencyStamps, out: &mut [u8; BOOK_TICKER_SIZE]);
pub fn decode_bookticker(buf: &[u8; BOOK_TICKER_SIZE]) -> anyhow::Result<(BookTicker, LatencyStamps)>;
pub fn encode_trade(tr: &Trade, stamps: &LatencyStamps, out: &mut [u8; TRADE_SIZE]);
pub fn decode_trade(buf: &[u8; TRADE_SIZE]) -> anyhow::Result<(Trade, LatencyStamps)>;

// topics.rs
pub const MSG_BOOKTICKER: &str = "bookticker";
pub const MSG_TRADE: &str = "trade";
pub const MSG_WEBBOOKTICKER: &str = "webbookticker";

pub struct TopicBuilder { /* Arc<[u8]> 캐시 */ }
impl TopicBuilder {
    pub fn build(exchanges: &[ExchangeId], symbols: &[Symbol]) -> Self;
    pub fn get(&self, ex: ExchangeId, kind: &'static str, sym: &Symbol) -> &[u8];
}

// frame.rs
// [type_len:u8][type:str][payload:bytes]
pub fn encode_frame(kind: &str, payload: &[u8], out: &mut Vec<u8>);
pub fn decode_frame(buf: &[u8]) -> anyhow::Result<(&str, &[u8])>;
```

## 계약

- **Encode 는 0 alloc.** `out` 은 caller 가 준 stack 또는 재사용 버퍼.
- **Decode 는 `Result`**. 손상된 바이트는 에러로 전파, panic 금지.
- `TopicBuilder` 는 startup 에서 1회 빌드. runtime add/remove 지원 안 함 (재시작으로 처리).
- `LatencyStamps` 는 `hft-time` 의 struct 를 그대로 사용하며, 메모리 레이아웃 불일치 시 컴파일 타임 `static_assertions` 로 검출.

## Phase 1 TODO

1. `static_assertions::const_assert_eq!(size_of::<LatencyStamps>(), LATENCY_STAMPS_SIZE)`
2. `encode_bookticker` / `decode_bookticker`:
   - `byteorder::LittleEndian` 사용
   - 심볼 null-padded 직렬화 (16B 넘으면 truncate, metric 으로 경고)
3. Trade 동일
4. `TopicBuilder` 구현. 내부적으로 `HashMap<(ExchangeId, &'static str, Symbol), Arc<[u8]>>` 으로 잡되, `get` 은 `&[u8]` 만 리턴 (Arc deref 없이).
5. `frame.rs` — subscriber 가 역으로 파싱할 때 사용. HWM 때문에 가끔 메시지가 truncate 될 수 있으니 길이 검증.
6. 기존 `gate_hft/data_publisher_rust/src/serializer.rs` 와 바이트 단위 호환성 테스트 — 같은 입력에 대한 출력 비교.
7. Criterion bench:
   - `encode_bookticker` < 200ns
   - `decode_bookticker` < 300ns
   - `TopicBuilder::get` < 10ns (HashMap lookup 1회)

## Anti-jitter 체크
- `encode_*` 는 inline 가능한 크기로 유지. `#[inline]` 붙일 것.
- decode 에러 분기는 `#[cold]`.
- `TopicBuilder::get` 의 HashMap 은 `ahash` 또는 `fxhash` (startup-only 이라 DoS 걱정 없음).

## 완료 조건
- [ ] bench encode < 200ns, decode < 300ns
- [ ] heap alloc 0 (dhat) for encode/decode
- [ ] 기존 gate_hft serializer 와 output 완전 일치 (한 개 테스트 벡터로 검증)
- [ ] `TopicBuilder` startup 시간 1000 symbol × 5 exchange × 3 kind = 15k 엔트리 < 10ms
