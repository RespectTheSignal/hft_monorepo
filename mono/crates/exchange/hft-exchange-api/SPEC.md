# hft-exchange-api — SPEC

## 역할
거래소 인터페이스의 trait 만. 구현은 `hft-exchange-<vendor>` crate 에서. 상세 근거는 `docs/adr/0002-exchange-trait.md`.

## 공개 API (Phase 1 최종)

```rust
pub type Emitter = Arc<dyn Fn(MarketEvent, LatencyStamps) + Send + Sync + 'static>;

#[async_trait]
pub trait ExchangeFeed: Send + Sync {
    fn id(&self) -> ExchangeId;
    fn role(&self) -> DataRole;
    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>>;
    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()>;
}

#[derive(Clone, Copy, Debug)]
pub enum OrderSide { Buy, Sell }

pub struct OrderRequest {
    pub exchange: ExchangeId,
    pub symbol: Symbol,
    pub side: OrderSide,
    pub qty: f64,
    pub price: Option<f64>,      // None = market
    pub client_id: Arc<str>,
}

pub struct OrderAck {
    pub exchange: ExchangeId,
    pub exchange_order_id: String,
    pub client_id: Arc<str>,
    pub ts_ms: i64,
}

#[async_trait]
pub trait ExchangeExecutor: Send + Sync {
    fn id(&self) -> ExchangeId;
    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck>;
    async fn cancel(&self, exchange_order_id: &str) -> anyhow::Result<()>;
}
```

## 계약

- `stream` 은 장애 시 **자체 재연결**해야 하고, `emit` 는 얼마든지 여러 번 호출 가능.
- `emit` 콜백 안에서 모든 hot 작업 수행. 콜백 외부로 `MarketEvent` 를 보내는 channel 만들지 말 것 (중간 큐 → jitter).
- `cancel` token 이 triggered 되면 `stream` 이 Ok 또는 Err 로 return.
- `emit` 는 **allocation-free 여야 함** — `MarketEvent` 는 struct value, `LatencyStamps` 도 value.
- `place_order` 는 idempotent 하지 않음. retry 는 order-gateway 가 책임.

## Phase 1 TODO

1. 시그니처 위처럼 고정. `async_trait` 매크로.
2. `cancel: CancellationToken` 타입은 `tokio_util::sync::CancellationToken` 재수출.
3. `Emitter` 가 `Fn(MarketEvent, LatencyStamps)` 로 받도록 현재 `lib.rs` 업데이트.
4. doc test: trivial impl (always-fail) 로 trait object 가 compile 되는지.
5. 각 vendor crate SPEC 에서 이 trait 을 참조.

## Anti-jitter 체크
- `Emitter` 는 `Arc<dyn Fn>` — trait object 1회 간접, hot path 에서 문제 없음.
- `emit` 안에서 publisher 가 serialize + zmq push 를 모두 수행. 이게 느리면 WS 읽기도 같이 밀림 → backpressure 자연스럽게 WS 로 전달.

## 완료 조건
- [ ] trait 이 compile 되고 `hft-testkit::MockFeed` 에서 impl 가능
- [ ] `cancel` 로 stream 종료 가능 검증 (integration test)
