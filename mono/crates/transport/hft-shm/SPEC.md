# hft-shm — SPEC

## 역할
Intra-host shared memory transport. ZMQ 보다 20-100x 낮은 지연 (p50 50-300ns) 을
제공하기 위한 **publisher ↔ subscriber ↔ strategy ↔ order-gateway** 간 fast path.

전체 설계는 `docs/architecture/SHM_DESIGN.md`, 하드웨어·커널 전제는
`docs/runbooks/INFRA_REQUIREMENTS.md` 참조.

## 공개 타입
- `QuoteSlotWriter` / `QuoteSlotReader` — seqlock 기반 BookTicker 최신값.
- `TradeRingWriter` / `TradeRingReader` — Aeron 스타일 SPMC broadcast.
- `OrderRingWriter` / `OrderRingReader` — SPSC order (Python → Rust).
- `SymbolTable` — (exchange, name) → u32 매핑, append-only.
- `exchange_to_u8` / `exchange_from_u8` — SHM 와이어 encoding.

## 파일 배치 (운영 규약)
- `/dev/shm/hft_quotes_v2` — 10,000 slot × 128B + header = ~1.3MB
- `/dev/shm/hft_trades_v2` — 2^20 frame × 128B + header = 128MB
- `/dev/shm/hft_orders_v2` — 16,384 frame × 128B + header = 2MB
- `/dev/shm/hft_symtab_v2` — 16,384 entry × 64B + header = 1MB

모두 파일 권한 `0660 hft:hft`, `mlock` 시도됨.

## 불변식
- Writer 는 한 영역당 **한 프로세스**. 여러 프로세스에서 동시에 쓰지 않는다.
- Reader 는 여러 프로세스 가능 (SPMC trade ring). 각자 cursor 관리.
- 파일 크기는 생성 시점에 고정. 재시작해도 동일한 크기만 허용.
- `ExchangeId` u8 와이어 매핑은 `exchange_to_u8` 로만 생성 (enum discriminant `as u8`
  은 사용 금지).

## Phase 상태
- **Phase 2 Track C (current)**: 실구현 완료. 단위/통합 테스트 + 벤치마크 포함.
- **Phase 3**: eventfd sidechannel (wakeup 없는 저전력 reader), huge page 2MB
  mandatory mode, RDMA 게이트웨이 (cross-host 확장이 필요할 때만).

## 컴파일 전제
- Linux (mlock / MAP_POPULATE / MADV_HUGEPAGE). macOS/Windows 는 기능 저하로 동작
  (advise/mlock no-op) 하지만 프로덕션 환경 아님.
- Rust 1.80+, `unsafe_op_in_unsafe_fn` 강제.

## Anti-jitter 체크리스트
- 모든 atomic load/store 는 `Acquire/Release/Relaxed`. `SeqCst` 사용 금지.
- 헤더와 body 는 cache-line (64B) 정렬.
- writer 가 접근하는 atomic (writer_seq / head) 과 reader 가 반복 load 하는 atomic
  (frame.seq / tail) 은 서로 다른 cacheline.
- `/dev/shm` 이 tmpfs 인지 boot-time 에 확인.
- ulimit memlock = unlimited.
- `isolcpus` 로 핫스레드 코어 격리.
