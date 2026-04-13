# questdb_uploader

data_subscriber의 IPC 소켓에서 Flipster bookticker 데이터를 수신하여 QuestDB에 ILP 프로토콜로 기록하는 애플리케이션.

## 데이터 흐름

```
data_subscriber (IPC Unix socket)
    |  framed binary messages
    v
questdb_uploader
    |  ILP (TCP port 9009)
    v
QuestDB (flipster_bookticker 테이블)
```

## 환경변수

`.env` 파일 또는 환경변수로 설정:

| 변수 | 필수 | 설명 | 기본값 |
|---|---|---|---|
| `IPC_SOCKET_PATH` | N | subscriber IPC 소켓 경로 | `/tmp/flipster_data_subscriber.sock` |
| `QDB_CLIENT_CONF` | N | QuestDB ILP 연결 문자열 | `tcp::addr=211.181.122.102:9009;` |
| `QDB_TABLE_NAME` | N | QuestDB 테이블명 | `flipster_bookticker` |
| `FLUSH_INTERVAL_MS` | N | QuestDB flush 주기 (ms) | `200` |
| `RUST_LOG` | N | 로그 레벨 | `info` |

## 빌드 및 실행

```bash
cargo build --release

# subscriber가 먼저 실행 중이어야 함
./target/release/questdb_uploader
```

## QuestDB 테이블 스키마

자동 생성됨 (ILP auto-create):

| 컬럼 | 타입 | 설명 |
|------|------|------|
| `symbol` | SYMBOL | 심볼명 (예: BTC-USDT-PERP) |
| `bid_price` | DOUBLE | 최우선 매수호가 |
| `ask_price` | DOUBLE | 최우선 매도호가 |
| `last_price` | DOUBLE | 최근 체결가 |
| `mark_price` | DOUBLE | 마크 가격 |
| `index_price` | DOUBLE | 지수 가격 |
| `timestamp` | TIMESTAMP | Flipster 서버 타임스탬프 (designated timestamp) |

## 프로젝트 구조

```
src/
  main.rs        # IPC 클라이언트 + QuestDB ILP writer
  protocol.rs    # FlipsterBookTicker 바이너리 역직렬화
```
