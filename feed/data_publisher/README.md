# data_publisher

Flipster exchange의 bookticker 데이터를 실시간으로 수집하고 QuestDB에 저장하는 애플리케이션.

## 기능

- Flipster WebSocket API로부터 실시간 ticker 데이터(bid/ask/last/mark/index price) 수신
- NTP 방식의 서버 시간 동기화를 통한 정밀 latency 측정
- 심볼 수에 따라 자동으로 WebSocket 연결 분할
- QuestDB ILP 프로토콜을 통한 tick 데이터 저장 (선택적)
- 종료 시 per-symbol latency 통계 출력 (min/avg/p50/p99/max)

## 환경변수

`.env` 파일 또는 환경변수로 설정:

| 변수 | 필수 | 설명 | 기본값 |
|---|---|---|---|
| `FLIPSTER_API_KEY` | Y | Flipster API key | - |
| `FLIPSTER_API_SECRET` | Y | Flipster API secret | - |
| `FLIPSTER_BASE_URL` | N | REST API base URL | `https://trading-api.flipster.io` |
| `FLIPSTER_WS_URL` | N | WebSocket URL | `wss://trading-api.flipster.io/api/v1/stream` |
| `QDB_CLIENT_CONF` | N | QuestDB client config string | - (persistence 비활성화) |
| `RUST_LOG` | N | 로그 레벨 | `info` |

## 빌드

```bash
cd feed/data_publisher
cargo build --release
```

## 실행

```bash
# .env 파일 설정 후 실행
cp .env.example .env  # API 키 입력
cargo run --release

# CLI 옵션
cargo run --release -- --help
cargo run --release -- -t 5          # WebSocket당 최대 5개 토픽
cargo run --release -- -n 10         # 상위 10개 심볼만 구독
cargo run --release -- -t 5 -n 20   # 옵션 조합
```

### QuestDB 연동

QuestDB가 실행 중일 때 `QDB_CLIENT_CONF` 환경변수를 설정하면 tick 데이터가 `flipster_bookticker` 테이블에 저장됩니다.

```bash
# QuestDB TCP (ILP)
export QDB_CLIENT_CONF="tcp::addr=localhost:9009;"

# QuestDB HTTP
export QDB_CLIENT_CONF="http::addr=localhost:9000;"
```

QuestDB에 접속할 수 없는 경우 경고 로그를 출력하고 persistence 없이 계속 실행됩니다.

## 프로젝트 구조

```
src/
  main.rs          # 엔트리포인트, CLI 파싱, 이벤트 루프
  lib.rs           # 모듈 re-export
  config.rs        # 환경변수 기반 설정
  error.rs         # 에러 타입 정의
  store.rs         # QuestDB ILP writer
  time_sync.rs     # 서버 시간 동기화 (NTP-style)
  flipster/
    mod.rs         # flipster 모듈
    auth.rs        # HMAC-SHA512 인증 서명
    models.rs      # API/WS 응답 모델
    rest.rs        # REST API 클라이언트
    ws.rs          # WebSocket 클라이언트
```
