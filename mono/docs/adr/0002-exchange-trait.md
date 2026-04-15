# ADR 0002: Exchange Trait 설계

- **Status**: Accepted
- **Date**: 2026-04-15
- **Author**: sigma13-K

## Context

기존 `gate_hft` 는 거래소별로 독립 바이너리 (`data_publisher_rust` = Gate 전용,
`futures_collector` = Binance/Bybit/Bitget/OKX) 로 분리돼 있고, 각 바이너리 안에
WS 연결·파싱·직렬화·송출 로직이 단단히 묶여 있었다.

문제:
- 새 거래소 추가 = 새 바이너리 복제 + publisher 파이프라인 재작성
- 테스트 시 거래소 mock 불가 (MockExchange 주입 불가)
- Gate 는 API/Web 이중 WS, Binance 는 단일 WS 처럼 거래소마다 특성이 다름에도
  공통 인터페이스가 없어 `if exchange == "gate"` 분기가 publisher 에 스며듦

## Decision

거래소 인터페이스를 `hft-exchange-api` crate 의 두 trait 으로 고정:

```rust
#[async_trait]
pub trait ExchangeFeed: Send + Sync {
    fn id(&self) -> ExchangeId;
    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>>;
    async fn stream(&self, symbols: &[Symbol], emit: Emitter) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ExchangeExecutor: Send + Sync {
    fn id(&self) -> ExchangeId;
    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck>;
    async fn cancel(&self, order_id: &str) -> anyhow::Result<()>;
}
```

핵심 결정:

1. **Feed 와 Executor 분리**
   시세 수집과 주문 실행은 수명주기·신뢰성 요구가 다르다. Feed 는 장애 시 재연결·
   재구독, Executor 는 장애 시 즉시 에러 전파 (재시도는 상위 strategy/order-gateway
   책임). 한 trait 에 묶으면 구현체가 비대해지고 mock 도 어려워짐.

2. **`stream()` 은 콜백 기반 (`Emitter = Arc<dyn Fn(MarketEvent) + Send + Sync>`)**
   Stream trait 반환 대신 콜백을 선택한 이유:
   - Gate WS 처럼 한 연결에서 book/trade/webbook 3종을 섞어 내보내는 경우 Stream<Item=T>
     으로는 타입을 하나로 못 묶음. `MarketEvent` enum + 콜백이 자연스러움
   - 콜백 안에서 zero-alloc 직렬화 + ZMQ push 를 바로 수행 가능 → 중간 channel 제거
   - 대신 backpressure 는 구현체 책임 (Gate 쪽은 WS 자체가 backpressure 를 흡수)

3. **`MarketEvent::WebBookTicker` 를 enum 값으로 승격**
   Gate 는 API WS (`BookTicker`) 와 Web WS (`WebBookTicker`) 에서 오는 값이 의미적으로
   같지만 latency 특성이 다르다. 같은 BookTicker 로 묶으면 downstream 에서 구분이
   불가능해 토픽 네이밍만으로 구분해야 했던 기존 문제 재발. enum 분기로 타입 레벨 분리.

4. **`ExchangeId` enum (String 이 아닌)**
   - 잘못된 거래소 이름 compile-time 검출
   - match 에서 non-exhaustive 경고 → 새 거래소 추가 시 누락 방지
   - serde rename_all = "lowercase" 로 config/로그와 호환

5. **`DataRole::Primary | Secondary`**
   Gate 의 API/Web 이중 WS 처럼 같은 거래소에서 여러 feed 를 돌릴 때 하나를 정규 소스로,
   다른 하나를 보조 (비교/백업) 로 표시. 구현체가 자기 role 을 필드로 갖고,
   publisher 는 topic 네이밍·QuestDB 태깅에 이 값을 반영.

## Consequences

**Positive**:
- publisher 는 `Box<dyn ExchangeFeed>` 만 받으면 됨 → 거래소 추가 = 새 crate + config 한 줄
- 테스트에서 `MockFeed` 로 주입해 latency 파이프라인·토픽 라우팅만 단위 테스트 가능
- Feed/Executor 가 분리돼 있어 "시세는 받지만 주문은 다른 경로" 같은 구성도 자연스러움

**Negative**:
- trait object (`Box<dyn ExchangeFeed>`) 사용 → vtable 1회 간접호출. HFT 에서는
  무시 가능 (emit 콜백 안의 직렬화·syscall 비용이 압도적). 핫패스는 콜백 내부이므로
  거래소 trait 자체의 dyn 비용은 문제되지 않음
- `async_trait` 매크로 → 각 async fn 이 `Pin<Box<dyn Future>>` 리턴. stream() 은
  수명주기 동안 1회만 호출되므로 heap 할당 1회. 허용 가능

**Neutral**:
- 기존 `futures_collector` 의 거래소별 로직은 각 `hft-exchange-<name>` crate 로
  1:1 이식. 파싱 로직은 재사용, WS 연결·재시도 wrapper 는 통일
- Flipster 는 Python 으로 별도 유지 (`strategy_flipster_python/`) → 이 trait 의
  대상 아님. Rust 편입 시점에 `hft-exchange-flipster` crate 추가

## Phase 매핑

- Phase 0 (현재): trait 시그니처만 존재. `todo!()` 혹은 빈 구현
- Phase 2: Binance / Gate 2개 구현체 작성 → publisher 실제 동작
- Phase 3: Bybit / Bitget / OKX 이식
- Phase 4: Executor 구현 (order-gateway 에서 사용)
