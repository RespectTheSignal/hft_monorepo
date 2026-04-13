# hft_monorepo

Flipster 거래소 시장 데이터 수집/배포 파이프라인.

## 아키텍처

```
Flipster WebSocket (494 symbols)
        |
        v
+-----------------------+
|   data_publisher      |   Flipster WS -> ZMQ PUB
|   tcp://0.0.0.0:7000  |   binary C-struct (96B)
+-----------+-----------+
            | ZMQ PUB/SUB
      +-----+------+
      |            |
      v            v
  Machine A    Machine B
+-----------+ +-----------------+
|subscriber | | subscriber      |
|    |      | |    |            |
| strategy  | | questdb_uploader|
+-----------+ |    |            |
              | QuestDB (9009)  |
              +-----------------+
```

## 컴포넌트

| 크레이트 | 경로 | 역할 |
|----------|------|------|
| `data_publisher` | `feed/data_publisher/` | Flipster bookticker WS 수신 -> ZMQ PUB 발행 |
| `data_subscriber` | `feed/data_subscriber/` | ZMQ SUB 수신 -> IPC (Unix socket)로 로컬 프로세스에 배포 |
| `questdb_uploader` | `feed/questdb_uploader/` | subscriber IPC에서 수신 -> QuestDB ILP로 기록 |

## 바이너리 프로토콜 (FlipsterBookTicker, 104 bytes)

publisher와 subscriber 간 공유되는 메시지 포맷:

```
Field               Offset  Size  Type      Description
symbol              0       32    [u8;32]   심볼명 (null-padded)
bid_price           32      8     f64 LE    최우선 매수호가
ask_price           40      8     f64 LE    최우선 매도호가
last_price          48      8     f64 LE    최근 체결가
mark_price          56      8     f64 LE    마크 가격 (청산 기준)
index_price         64      8     f64 LE    지수 가격
server_ts_ns        72      8     i64 LE    Flipster 서버 타임스탬프 (ns)
publisher_recv_ns   80      8     i64 LE    publisher WS 수신 시각 (ns)
publisher_sent_ns   88      8     i64 LE    publisher ZMQ 발행 시각 (ns)
subscriber_recv_ns  96      8     i64 LE    subscriber 수신 시각 (ns, subscriber가 채움)
```

ZMQ multipart 메시지: `[topic][payload]`
- Topic: `flipster_bookticker_{SYMBOL}` (예: `flipster_bookticker_BTC-USDT-PERP`)
- Payload: 104 bytes

## 타임스탬프 체인

```
Flipster server_ts (거래소 원본, ns)
  -> publisher_recv_ns (publisher가 WS에서 수신한 시각)
    -> publisher_sent_ns (publisher가 ZMQ로 발행한 시각)
      -> subscriber_recv_ns (subscriber가 수신한 시각)
```

## 빌드

```bash
# 전체 빌드
cd feed/data_publisher && cargo build --release
cd feed/data_subscriber && cargo build --release
```

시스템 의존성: `libzmq3-dev` (Ubuntu: `apt install libzmq3-dev`)

## 빠른 시작

```bash
# 1. publisher 실행
cd feed/data_publisher
cp .env.example .env  # API key 설정
./target/release/data_publisher

# 2. subscriber 실행 (다른 터미널)
cd feed/data_subscriber
./target/release/data_subscriber

# 3. IPC 클라이언트 연결
#    /tmp/flipster_data_subscriber.sock 에 Unix socket으로 연결
#    등록: "REGISTER:<process_id>:<symbol1>,<symbol2>,...\n"
```
