# Latency Budget — 50ms end-to-end

**정의**: "end-to-end" = `exchange_server_ms` (거래소가 WS 프레임을 내보낸 시각)
에서 `consumed_ms` (strategy 또는 subscriber 가 이벤트를 loop 안에서 읽어낸 시각)
까지의 wall-clock 차이.

**대상**: BookTicker / Trade / WebBookTicker 세 종.

**요구사항**:
- p50 < 25ms
- p99 < 40ms
- **p99.9 < 50ms (spike 0, 이게 이 프로젝트의 1차 KPI)**

## Stage 분할과 각 stage 의 예산

| # | Stage | 측정 필드 (LatencyStamps) | 예산 (ms) | 제어 가능? | 비고 |
|---|---|---|---|---|---|
| 1 | Exchange internal → Exchange edge | `exchange_server_ms` 자체 생성 | — | 불가 | 거래소 내부 처리. 관측만 |
| 2 | Exchange edge → 우리 WS 소켓 | `ws_received_ms - exchange_server_ms` | 15~25 | 불가 | 네트워크. PoP 선택·co-lo 만 개입 |
| 3 | WS 파싱 → 직렬화 직전 | `(직렬화 시작) - ws_received_ms` | < 0.3 | 가능 | `serde_json::from_slice` + validate |
| 4 | 직렬화 → worker PUSH | `pushed_ms - ws_received_ms` 의 잔여 | < 0.2 | 가능 | zero-copy C-struct write |
| 5 | Worker PUSH → aggregator PULL | `(agg_received) - pushed_ms` | < 0.3 | 가능 | inproc/IPC ZMQ. HWM 100K |
| 6 | Aggregator PUB → subscriber SUB | `subscribed_ms - published_ms` | < 1.0 | 가능 | TCP ZMQ. SUB 쪽 커널 버퍼 포함 |
| 7 | Subscriber 파싱 → consumer 획득 | `consumed_ms - subscribed_ms` | < 0.3 | 가능 | C-struct read (memcpy) |
| 8 | (주문 경로) Strategy decision | strategy 내부 span | < 1.0 | 가능 | 사용자 전략 로직 |
| 9 | (주문 경로) order-gateway → exchange | — | 15~25 | 불가 | 네트워크 |

**합**: 네트워크 2·9 (약 30~50ms, 우리 제어 밖) + 우리 영역 3·4·5·6·7 (< 2.1ms)

## 해석

50ms 를 못 맞추는 원인은 **거의 항상 네트워크** (stage 2/9). 우리가 제어할 수 있는
구간의 예산 2.1ms 는 사실상 충분히 여유 있다. **문제가 되는 건 평균이 아니라 tail.**

p99.9 spike 가 나는 전형 경로 (우리 영역):
- Tokio runtime 의 다른 task 가 hot path 를 블로킹 (stage 3·4 에 수 ms 추가)
- ZMQ HWM 초과 → 블로킹 send (stage 5·6 에 수십 ms 추가)
- QuestDB 로 가는 쓰기가 같은 thread 에서 돌면서 GC-유사 스파이크 (stage 3~5 전체에 수십 ms)
- mimalloc 아닌 기본 allocator 의 arena contention (stage 3 에 급증)
- Tracing 의 `info!` 가 file writer lock 을 붙잡음 (stage 3~7 어디든 수 ms)

각 원인의 구체적 대응은 `jitter-playbook.md` 참조.

## 측정 지점과 책임

| 필드 | 채우는 crate | 언제 |
|---|---|---|
| `exchange_server_ms` | 각 `hft-exchange-<name>` | WS 메시지 파싱 시 거래소가 준 `T` / `E` 필드 복사 |
| `ws_received_ms` | 각 `hft-exchange-<name>` | WS 프레임 수신 콜백 진입 직후 (`Clock::now_ms()`) |
| `serialized_ms` | `services/publisher` | `hft-protocol` 로 직렬화 직후 |
| `pushed_ms` | `services/publisher` worker | ZMQ `send` 호출 직전 |
| `published_ms` | `services/publisher` aggregator | ZMQ PUB `send` 직전 |
| `subscribed_ms` | `services/subscriber` | SUB recv 직후 |
| `consumed_ms` | strategy / tools/latency-probe | 이벤트를 자기 loop 에서 꺼낸 직후 |

이 timestamp 들은 wire payload 에 같이 실려 다닌다 (`hft-protocol` 의 고정 오프셋).
downstream 에서는 현재 시각과의 차로 stage 별 지연을 복원해 `hft-telemetry` 의
HDR histogram 에 기록한다.

## SLO 달성 실패 시 디버그 순서

1. `tools/latency-probe` 로 stage 별 p50/p99/p99.9 뽑음
2. p99.9 와 p50 의 차가 큰 stage 찾음 → 그게 jitter 원인
3. 해당 stage 의 crate `SPEC.md` 의 "Anti-jitter 체크" 섹션부터 검증
4. 필요하면 `hft-telemetry` 의 flamegraph / tokio-console 켜서 blocking 지점 확인
