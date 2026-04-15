# `hft-shm` — SHM 기반 통신 레이아웃 설계

> **⚠️ 상위 권위 문서 알림 (2026-04-15 추가)**
> 본 문서는 레이아웃(프레임/슬롯/링)과 seqlock/SPSC/SPMC 규약의 원본이다. **유효.**
> 단 실제 배포의 **IPC 경계 (어디서 어디로, 어떤 백엔드)** 는
> `docs/architecture/MULTI_VM_TOPOLOGY.md` 에서 결정된다. 충돌 시 그 문서를 따른다.
> 요약: `/dev/shm` 단일 호스트 → **ivshmem 멀티-VM** 으로 전환.
> ADR: `docs/adr/0003-multi-vm-ivshmem-topology.md`.

> 목표: **publisher → strategy → order-gateway 간 통신을 sub-μs 대로 끌어내림.** ZMQ TCP 12-30μs → SHM busy-poll 50-300ns.

작성일: 2026-04-15. Phase 2 Track C 의 핵심 컴포넌트.

---

## 1. 왜 SHM 인가 — 지연 비교

| 방식 | 전형적 p50 | 비고 |
|---|---|---|
| ZMQ TCP (loopback) | 12–30 μs | 지금까지 Phase 1 선택지 |
| ZMQ IPC (UDS 기반) | 7–20 μs | syscall × 2 |
| raw UDS (SEQPACKET) | 5–12 μs | 여전히 syscall |
| SHM + eventfd wakeup | 0.5–2 μs | syscall 은 wakeup 에만 |
| **SHM + busy-poll** | **0.05–0.3 μs** | syscall 제로 |

우리가 설계하는 경로는 **SHM + busy-poll** (consumer 가 active polling). CPU 는 쓰지만, 격리된 코어에서만 돌기 때문에 다른 워크로드에 영향 없음.

---

## 2. 세 개의 SHM 영역

### 2.1 Quote slot (`/dev/shm/hft_quotes_v2`) — seqlock

- **용도**: BookTicker (best bid/ask) 의 **최신값만** 공유. history 아님.
- 구조: `[SlotHeader] + [QuoteSlot; N]`, 각 slot 128B.
- N = 10,000 (전 거래소 × 전 symbol, 여유 포함).
- **writer**: publisher aggregator (1 쓰레드)
- **reader**: strategy, order-gateway, 기타 모니터링 (N 개, 서로 다른 코어)
- 쓰기 방식: **seqlock** — `seq` 가 홀수 동안 쓰기 중, 짝수면 완료. reader 는 seq 가 같고 짝수인지 확인.

```
SlotHeader (64B aligned, 128B)
  magic: u64              # 0x53484D5F51544501 ("SHM_QT\1")
  version: u32            # 2
  slot_count: u32         # N
  element_size: u32       # 128
  writer_pid: u32
  created_ns: u64
  reserved: [u8; ...]

QuoteSlot (128B aligned, 128B each)
  seq: AtomicU64         # even=stable, odd=writing
  exchange_id: u8
  _pad1: [u8; 7]
  symbol_idx: u32        # symbol table 의 index
  _pad2: [u8; 4]
  bid_price_raw: i64     # Price::raw (scaled i64)
  bid_size_raw: i64
  ask_price_raw: i64
  ask_size_raw: i64
  event_ns: u64          # exchange timestamp
  recv_ns: u64           # 로컬 수신 시각 (CLOCK_MONOTONIC)
  pub_ns: u64            # writer 가 기록 직전 시각
  _pad3: [u8; ...]
  (총 128B)
```

### 2.2 Trade ring (`/dev/shm/hft_trades_v2`) — SPMC broadcast

- **용도**: Trade 이벤트 스트림. 개별 이벤트가 모두 의미 있으므로 "slot" 이 아닌 "ring" 필요.
- 구조: `[RingHeader] + [TradeFrame; 2^20]`, frame 128B.
- 총 128MB.
- **writer**: publisher aggregator (단일). `seq` 단조 증가.
- **readers**: N 개. 각자 독립 cursor. writer 는 reader 를 절대 기다리지 않는다 (뒤쳐진 reader 는 **lap** 되어 drop 감지).

```
RingHeader (64B aligned, 128B)
  magic: u64              # 0x53484D5F54524401 ("SHM_TRD\1")
  version: u32
  capacity_mask: u32      # (2^k) - 1
  element_size: u32
  writer_pid: u32
  created_ns: u64
  writer_seq: AtomicU64   # 64B align, cacheline 단독
  _pad: [u8; ...]

TradeFrame (128B, cacheline aligned)
  seq: AtomicU64         # writer_seq 에서 복사. reader 가 lap 감지에 사용.
  exchange_id: u8
  _pad1: [u8; 3]
  symbol_idx: u32
  price_raw: i64
  size_raw: i64           # negative = sell (legacy 규약)
  trade_id: i64
  event_ns: u64
  recv_ns: u64
  pub_ns: u64
  flags: u32              # bit0 = is_internal(block-trade)
  _pad2: [u8; ...]
  (총 128B)
```

**SPMC lap 프로토콜** (Disruptor / Aeron 스타일):
- reader 의 cursor `r` 는 "다음에 읽을 seq". writer 의 `w` 는 "다음에 쓸 seq".
- reader: `slot = frames[r & mask]`. `slot.seq.load(Acquire)` 가 `r` 이면 정상 → 프레임 읽고 `r += 1`.
- `slot.seq` 가 `r + capacity` 이상이면 **lap 당함** (writer 가 한 바퀴 돌면서 덮어씀) → drop counter 증가 + cursor 점프 `r = (writer_seq - capacity + 1).max(r)`.
- `slot.seq == r - capacity` (= 이전 랩의 잔해) 이면 아직 쓰이지 않음 → wait.

### 2.3 Order ring (`/dev/shm/hft_orders_v2`) — SPSC

- **용도**: Python strategy → Rust order-gateway 주문 전달.
- 구조: `[RingHeader] + [OrderFrame; 16,384]`, 128B frame.
- 2 MB.
- Python side: writer, Rust side: reader. 한 쪽씩이라 **SPSC Lamport** 패턴이면 충분.

```
OrderFrame (128B)
  seq: u64
  kind: u8                 # 0=Place, 1=Cancel
  exchange_id: u8
  symbol_idx: u32
  side: u8                 # 0=Buy, 1=Sell
  tif: u8                  # 0=GTC, 1=IOC, 2=FOK
  ord_type: u8             # 0=Limit, 1=Market
  _pad1: [u8; 1]
  price_raw: i64
  size_raw: i64
  client_id: u64
  ts_ns: u64
  aux_u64: [u64; 5]        # payload 여유 (cancel 시 exchange_order_id 등)
  (총 128B)
```

Ack 는 역방향 SPSC (Rust → Python) 로 같은 포맷 재활용 예정 (별도 `hft_order_acks_v2`).

### 2.4 Symbol table (`/dev/shm/hft_symtab_v2`)

- 문자열 symbol 을 매번 비교하기 싫어서 `symbol_idx: u32` 로 주고받는다.
- 구조: `[SymTabHeader] + [SymbolEntry; N]`, entry 64B.
- writer: publisher (startup 시 1회 or 필요 시 append), reader: strategy / order-gateway.
- symbol 은 append-only (삭제 없음) → atomic `count` 만 갱신.

```
SymTabHeader (64B)
  magic: u64                 # 0x53484D5F53594D01 ("SHM_SYM\1")
  version: u32
  capacity: u32
  count: AtomicU32           # 현재 등록된 symbol 수
  created_ns: u64
  _pad: [u8; ...]

SymbolEntry (64B)
  exchange_id: u8
  _pad1: [u8; 3]
  name_len: u32              # <= 56
  name: [u8; 56]             # UTF-8, null 패딩
```

---

## 3. Rust API 스케치

```rust
// quote slot
let quotes = QuoteSlotWriter::create("/dev/shm/hft_quotes_v2", 10_000)?;
quotes.write(symbol_idx, &QuoteUpdate { bid, ask, ... })?;

let quotes_r = QuoteSlotReader::open("/dev/shm/hft_quotes_v2")?;
let snap: QuoteSnapshot = quotes_r.read(symbol_idx)?;   // torn-read 방지

// trade ring
let trades_w = TradeRingWriter::create("/dev/shm/hft_trades_v2", 1 << 20)?;
trades_w.publish(&trade_frame);

let mut trades_r = TradeRingReader::open("/dev/shm/hft_trades_v2")?;
while let Some(frame) = trades_r.try_consume() { ... }

// order ring (Rust reader)
let mut orders = OrderRingReader::open("/dev/shm/hft_orders_v2")?;
while let Some(o) = orders.try_consume() { ... }

// symbol table
let symtab = SymbolTable::open_or_create("/dev/shm/hft_symtab_v2", 16_384)?;
let idx = symtab.get_or_intern(ExchangeId::Gate, "BTC_USDT")?;
```

모든 타입은 `Send + Sync` (`Arc` 로 공유 가능), 내부는 `UnsafeCell<*mut u8>` + atomic 오프셋 접근.

---

## 4. Wake-up 전략

Phase 2 의 기본 선택: **busy-poll**. 격리된 전략 코어에서 `while !cancel { try_consume(); std::hint::spin_loop(); }` 가 가장 단순하고 가장 빠름.

| 전략 | 지연 | CPU | 언제 |
|---|---|---|---|
| busy-poll | 50–300 ns | 100% | 핫패스 (strategy 주 루프, order-gateway 주문 경로) |
| spin + backoff (PAUSE → yield → futex) | 300 ns – 3 μs | 적응적 | 거래소별 executor 의 보조 타스크 |
| eventfd sidechannel | 2–10 μs | 0 | 로깅, 주기 flush |

eventfd 는 wake-up 지연 자체가 μs 단위여서 "응답 시간을 ms 로 경쟁하는" 주 경로에 쓰면 의미 없음. 단, ops / telemetry 루프엔 유용.

---

## 5. 크래시 / 재시작 동작

### 5.1 writer 재시작
- publisher 가 죽었다 살아나면 기존 shm 을 `MAP_SHARED` 로 재매핑. header 의 `writer_pid` 를 자기 pid 로 교체.
- quote slot: 기존 데이터가 stale 하면 reader 쪽에서 `pub_ns` 로 판단. writer 는 slot 을 reset 하지 않음 (다음 write 로 덮어쓸 뿐).
- trade ring: `writer_seq` 를 그대로 이어서 증가. reader 쪽은 gap 을 감지하면 drop counter 만 올림.
- symbol table: append-only 이므로 변화 없음.

### 5.2 reader 재시작
- `open()` 시 `writer_seq` 부터 읽기 시작 → 이전 데이터 유실 용인. (backfill 은 QuestDB 에서 따로 쿼리.)

### 5.3 비정상 종료 감지
- 각 shm 은 `writer_pid`, `created_ns` 를 헤더에 가짐. reader 는 **30초** 동안 `pub_ns`/`writer_seq` 변화가 없으면 writer down 으로 간주하고 알람.

---

## 6. 보안 / 격리

- shm 파일은 `0660 hft:hft` 로 생성. 다른 유저 접근 차단.
- `/dev/shm` 이 다른 컨테이너와 공유되지 않도록 컨테이너 사용 시 private tmpfs 마운트.
- `mlock` 으로 swap 방지 (ulimit memlock unlimited 필요).

---

## 7. 성능 타깃 (Phase 2 수락 기준)

- **publisher → reader 1-hop p50 ≤ 300 ns**, p99 ≤ 1 μs (격리 코어 위에서, 128B frame).
- **strategy → order-gateway p50 ≤ 500 ns** (order ring).
- **drop rate** (SPMC lap) ≤ 1e-6 under sustained 200k msg/s per exchange.

벤치마크: `hft-shm/benches/vs_uds.rs` — criterion 기반.

---

## 8. 관련 파일

- 구현: `crates/transport/hft-shm/src/`
- 벤치: `crates/transport/hft-shm/benches/vs_uds.rs`
- Python reader: `feed/python/shm_reader.py` (strategy 쪽)
- Ops 튜닝: `docs/runbooks/INFRA_REQUIREMENTS.md`
- 설정: `hft-config::ShmConfig` (Phase 2 추가)
