# tools/questdb-export — SPEC

## 역할
publisher PUB 을 구독해서 QuestDB 로 영속화하는 **standalone 프로세스**. hot path 에서 QuestDB 를 분리하는 이유의 핵심 구현.

## 구조

```
 tcp ZMQ SUB ──► decode ──► hft_storage::QuestDbSink ──► QuestDB (ILP 9009)
                                   │
                                   └─► spool file on connect failure
```

## Phase 1 TODO

1. main: config 로드, ZMQ SUB, hft-storage Sink 생성, loop.
2. batch tick: 100ms 마다 `sink.maybe_flush()`.
3. graceful shutdown: SIGTERM → drain 남은 in-flight + force_flush.
4. 스키마 init 은 startup 에 (QuestDB PG wire 로) 1회.
5. systemd unit + docker-compose stanza (deploy/).

## 계약
- publisher 의 PUB 과 같은 subscription pattern. 모든 토픽 구독 (`sub.subscribe("")`).
- QuestDB 장애 시 ILP retry + spool — publisher 에 backpressure 전달 안 함 (독립 프로세스).
- latency_stages 테이블에도 `LatencyStamps` 모든 필드 기록 → 장기 관측 가능.

## Anti-jitter 체크
- 이 프로세스는 hot path 가 아님 — 자원은 다른 core 에 배치.
- SUB recv 블로킹은 OK (이 프로세스 안에 다른 hot 작업이 없음).

## 완료 조건
- [ ] publisher 100K msg/s + 본 툴 flush 동시 24h 동안 publisher 의 hot p99.9 무영향
- [ ] QuestDB 프로세스 재시작 시 spool 에 쌓이고 자동 복구
