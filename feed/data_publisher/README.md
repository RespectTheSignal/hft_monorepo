# data_publisher

Flipster 거래소의 bookticker 데이터를 실시간으로 수집하여 ZMQ PUB 소켓으로 발행하는 애플리케이션.

## 기능

- Flipster WebSocket API로부터 실시간 ticker 데이터(bid/ask/last/mark/index price) 수신
- NTP 방식의 서버 시간 동기화를 통한 정밀 latency 측정
- 심볼 수에 따라 자동으로 WebSocket 연결 분할 (기본 10개/연결)
- Flipster partial update 병합 (per-symbol state merge)
- ZMQ PUB 소켓으로 바이너리 C-struct(96B) 발행
- 429 Rate Limit 시 exponential backoff (3s -> 60s)
- 종료 시 per-symbol latency 통계 출력 (min/avg/p50/p99/max)

## 데이터 흐름

```
Flipster WS (N connections)
    |  JSON partial updates
    v
TickEvent (mpsc channel)
    |
    v
TickerState (per-symbol merge)
    |  bid+ask 모두 수신 후
    v
ZmqPublisher
    |  binary C-struct (96B)
    v
ZMQ PUB socket (tcp://0.0.0.0:7000)
    |  multipart: [topic][payload]
    v
Subscribers (ZMQ SUB)
```

## 환경변수

`.env` 파일 또는 환경변수로 설정:

| 변수 | 필수 | 설명 | 기본값 |
|---|---|---|---|
| `FLIPSTER_API_KEY` | Y | Flipster API key | - |
| `FLIPSTER_API_SECRET` | Y | Flipster API secret | - |
| `FLIPSTER_BASE_URL` | N | REST API base URL | `https://trading-api.flipster.io` |
| `FLIPSTER_WS_URL` | N | WebSocket URL | `wss://trading-api.flipster.io/api/v1/stream` |
| `FLIPSTER_DATA_PORT` | N | ZMQ PUB 포트 | `7000` |
| `FLIPSTER_ZMQ_PUB_ADDR` | N | ZMQ PUB 주소 (포트 대신 직접 지정) | `tcp://0.0.0.0:$PORT` |
| `RUST_LOG` | N | 로그 레벨 | `info` |

## 빌드 및 실행

```bash
cargo build --release
./target/release/data_publisher

# CLI 옵션
./target/release/data_publisher --help
./target/release/data_publisher -t 5          # WebSocket당 최대 5개 토픽
./target/release/data_publisher -n 10         # 상위 10개 심볼만 구독
./target/release/data_publisher -t 5 -n 20   # 옵션 조합
```

## ZMQ 발행 포맷

Multipart 메시지 `[topic][payload]`:

- **Topic**: `flipster_bookticker_{SYMBOL}` (예: `flipster_bookticker_BTC-USDT-PERP`)
- **Payload**: 96 bytes FlipsterBookTicker C-struct (little-endian)

```
Offset  Size  Type      Field
0       32    [u8;32]   symbol (null-padded)
32      8     f64 LE    bid_price
40      8     f64 LE    ask_price
48      8     f64 LE    last_price
56      8     f64 LE    mark_price
64      8     f64 LE    index_price
72      8     i64 LE    server_ts_ns (Flipster 서버 타임스탬프, ns)
80      8     i64 LE    publisher_sent_ns (발행 시각, ns)
88      8     i64 LE    subscriber_recv_ns (0, subscriber가 채움)
```

### ZMQ 소켓 설정

- `sndhwm`: 100,000 (send high-water mark)
- `sndbuf`: 4 MB (OS-level send buffer)
- `linger`: 0 (즉시 종료)
- 내부 버퍼: 65,536 메시지 (sync channel)

## Partial Update 처리

Flipster는 변경된 필드만 전송 (partial update). publisher는 per-symbol `TickerState`를 유지하며 매 메시지마다 merge한 뒤, bid와 ask가 모두 수신된 후에만 ZMQ로 발행합니다.

## 프로젝트 구조

```
src/
  main.rs          # 엔트리포인트, CLI 파싱, 이벤트 루프, partial merge
  lib.rs           # 모듈 re-export
  config.rs        # 환경변수 기반 설정
  error.rs         # 에러 타입 정의
  publisher.rs     # ZMQ PUB 소켓 + 바이너리 직렬화
  time_sync.rs     # 서버 시간 동기화 (NTP-style)
  flipster/
    mod.rs         # flipster 모듈
    auth.rs        # HMAC-SHA512 인증 서명
    models.rs      # API/WS 응답 모델
    rest.rs        # REST API 클라이언트
    ws.rs          # WebSocket 클라이언트 (TCP_NODELAY)
```
