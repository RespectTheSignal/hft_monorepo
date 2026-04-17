# ADR 0004 — Result Return Path: ZMQ 유지, SHM Result Ring 보류

Status: **Accepted** (2026-04-17)
Supersedes: 없음. 보완: ADR-0003 (multi-VM topology), check.md Phase 2 E
Owner: jeong

## Context

Phase 2 E 에서 주문 결과(order result) 반환 경로를 구현하면서 두 가지 선택지가
있었다:

1. **ZMQ reverse path** — gateway 가 `ZMQ PUSH` 로 result wire 를 strategy
   `ZMQ PULL` 로 보내는 방식. 구현 완료 (commit 5f95b93).
2. **SHM result ring** — ivshmem shared region 안에 gateway→strategy SPSC
   result ring 을 추가하는 방식. 미구현.

추가로, heartbeat 경로 (commit ffbfb29) 도 ZMQ result path 를 재사용해서
STATUS_HEARTBEAT=255 wire 를 전송한다.

결정이 필요한 범위:
- result 반환에 SHM ring 이 필요한가?
- heartbeat 을 SHM 으로 옮길 필요가 있는가?
- fill streaming (실시간 체결 통지) 도 같은 경로를 쓸 수 있는가?

## Decision

### 1. Order result 반환: ZMQ 경로 유지, SHM result ring 은 보류

**근거:**

- **빈도 차이**: market data 는 수천~수만 msg/s 이므로 SHM 이 정당하지만,
  order result 는 현실적으로 수십 msg/s 미만. ZMQ TCP 의 20–50μs 가
  주문 결과 처리에는 충분하다.
- **복잡도 대비 이득**: SHM result ring 을 추가하면 shared region layout
  변경 (전체 infra reboot 이벤트, ADR-0003 §Decision 5), writer/reader
  양쪽 구현, 그리고 다중 strategy VM 에 대한 fan-out 설계가 필요하다.
  현 단계에서 이 복잡도를 정당화할 latency 이득이 없다.
- **fan-out 문제**: market data 는 SPMC (하나의 publisher → 여러 strategy
  reader) 이지만, result 는 gateway → 특정 strategy 로 라우팅돼야 한다.
  SHM 으로 이를 구현하려면 strategy 별 result ring 이 필요하고,
  이는 shared region 크기와 ring 수를 폭발시킨다.

**보류 조건** (이 조건이 충족되면 재검토):
- 초당 주문 빈도가 1,000+/s 를 넘어 ZMQ overhead 가 p99 latency budget 을
  침범할 때.
- fill streaming (거래소 WS user stream) 을 result path 로 통합할 때
  빈도가 크게 올라갈 수 있다 — 아래 §3 참조.

### 2. Heartbeat: ZMQ 경로 유지

- heartbeat 은 1Hz (result_heartbeat_interval_ms=1000). SHM 전환 이득 = 0.
- ZMQ channel 건전성을 겸하는 side benefit 이 있다: ZMQ path 가 죽으면
  heartbeat 도 끊기므로 transport 장애 자체를 감지한다.
- 만약 SHM result ring 을 나중에 도입하면, heartbeat 만 SHM 으로 옮기는 것은
  의미 없다 (결과 전체가 옮겨야 하므로).

### 3. Fill streaming (거래소 WS user stream → strategy)

현재 구현 안 됨. 로드맵 위치: Phase 2 후반 또는 Phase 3 초입.

- Gate WS user stream 으로 실시간 체결/포지션 업데이트를 받아
  `StrategyControl::FillUpdate` 로 전달하는 것이 목표.
- 예상 빈도: 초당 수십 건 (체결 빈도 의존). ZMQ result path 에 태울 수 있다.
- `OrderResultWire` 128B 포맷에 fill 정보를 담을 수 있는지는 ADR-0005
  (wire format 단일화) 에서 결정한다.
- **결론**: fill streaming 은 ZMQ result path 를 재사용하되, wire 안에
  status code 로 분기한다 (STATUS_FILL = TBD, STATUS_HEARTBEAT = 255 과
  같은 패턴).

## Alternatives Considered

| 대안 | 장점 | 기각 이유 |
|------|------|-----------|
| SHM result ring (SPSC, strategy 별 1개) | sub-μs latency | 현 주문 빈도에서 불필요. shared region layout 변경 = 전체 reboot. |
| SHM result ring (SPMC, broadcast) | layout 변경 1회 | result 는 특정 strategy 에만 전달돼야 함. broadcast 는 waste. |
| Redis Pub/Sub | 기존 infra 재활용 | 100μs+ latency. 새 의존성. |

## Consequences

- `OrderResultWire` 128B + ZMQ PUSH/PULL 이 result 반환의 단일 경로로 확정된다.
- shared region layout (`SHM_DESIGN.md`) 에 result ring 은 추가하지 않는다.
- fill streaming 구현 시 `OrderResultWire` 에 fill-specific status code 를
  추가하고 동일 ZMQ channel 을 태운다.
- SHM result ring 이 필요해지는 시점에 ADR-0004-v2 를 작성한다.
- check.md 의 "ADR-0004 draft" 항목을 `[x]` 로 전환한다.
