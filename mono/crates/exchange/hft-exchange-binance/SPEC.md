# hft-exchange-binance — SPEC

## 역할
Binance USDT-M futures WS 에서 BookTicker + AggTrade 수신, `MarketEvent` 로 emit. Phase 2 구현 대상 (Phase 1 은 스캐폴드).

## 공개 API

```rust
pub struct BinanceFeed {
    cfg: BinanceConfig,      // from hft_config::ExchangeConfig
    clock: Arc<dyn Clock>,
    http: reqwest::Client,    // symbol list fetch 에만
}

#[async_trait]
impl ExchangeFeed for BinanceFeed { /* ... */ }
```

## WS 엔드포인트 (구현 참고)
- BookTicker stream: `wss://fstream.binance.com/stream?streams=<sym>@bookTicker/...`
- AggTrade stream: `wss://fstream.binance.com/stream?streams=<sym>@aggTrade/...`
- REST (available symbols): `GET /fapi/v1/exchangeInfo`

## emit 책임 필드

| 거래소 필드 | `MarketEvent`/`LatencyStamps` 필드 |
|---|---|
| `T` (transaction time) | `server_time_ms`, `stamps.exchange_server_ms` |
| `E` (event time) | `event_time_ms` |
| 프레임 수신 시각 | `stamps.ws_received_ms` = `clock.now_ms()` |
| `b`,`a`,`B`,`A` | BookTicker fields |
| `p`,`q`,`T`,`m` (aggTrade) | Trade fields |

`stamps.serialized_ms` 등 하류 stamp 는 publisher 가 채움.

## Phase 1 TODO
- [ ] 스캐폴드 유지. `lib.rs` 에 타입만.
- [ ] Phase 2 에서 `tokio-tungstenite` 기반 stream 구현.

## Phase 2 TODO (참고용, 미리 명시)
1. `tokio-tungstenite` WS 연결, 자동 reconnect (backoff `cfg.reconnect_backoff_ms`).
2. 심볼 리스트 한 연결당 200 cap (Binance 제한 확인). 초과 시 복수 WS.
3. ping/pong — 서버 ping 9분 간격, 10분 무응답 시 끊김. 우리도 주기 ping.
4. JSON 파싱은 `simd-json` 또는 `sonic-rs` (serde_json 보다 빠름, 벤치 후 결정).
5. emit 콜백 안에서 zero-alloc. SIMD 파서가 scratch buffer 재사용해야 함.

## Anti-jitter 체크
- WS 는 bg 가 아니라 hot runtime 에 올림. 이유: `emit` 안에서 publisher 가 직렬화 + PUSH 하므로 hot 일관성 유지.
- 재연결 백오프 동안은 (장애 상태) drop metric 만 증가. hot p99.9 영향 없음 (이미 이벤트 안 오는 중).
- `reqwest` (REST 호출) 은 bg runtime + startup 에만.

## 완료 조건 (Phase 1)
- [ ] `BinanceFeed::new(cfg)` compile
- [ ] `impl ExchangeFeed` trait bound 만족

## 완료 조건 (Phase 2)
- [ ] 500 symbol 24h 스트리밍 동안 reconnect ≤ 3회, drop 0
- [ ] publisher 기준 stage 3~4 p99.9 < 1ms
