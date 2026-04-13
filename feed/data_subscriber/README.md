# data_subscriber

data_publisher의 ZMQ PUB 소켓에서 Flipster bookticker 데이터를 수신하여 로컬 프로세스에 IPC (Unix domain socket)로 배포하는 애플리케이션.

## 기능

- ZMQ SUB 소켓으로 publisher에서 바이너리 bookticker 데이터 수신
- Unix domain socket IPC 서버로 로컬 프로세스에 메시지 배포
- 클라이언트별 심볼 필터링 (등록 시 구독 심볼 지정)
- per-client writer thread (느린 클라이언트가 다른 클라이언트를 블로킹하지 않음)
- `subscriber_recv_ns` 타임스탬프 in-place 주입 (offset 88)
- publisher -> subscriber latency 모니터링 (p50/p99/max)

## 데이터 흐름

```
ZMQ PUB (data_publisher, tcp://IP:7000)
    |
    v
ZMQ SUB (topic: "flipster_bookticker_*")
    |
    v
stamp subscriber_recv_ns (offset 88)
    |
    v
IPC broadcast (/tmp/flipster_data_subscriber.sock)
    |
    +---> strategy (Machine A)
    +---> questdb_uploader (Machine B)
```

## 환경변수

`.env` 파일 또는 환경변수로 설정:

| 변수 | 필수 | 설명 | 기본값 |
|---|---|---|---|
| `DATA_SERVER_IP` | N | publisher IP 주소 | `127.0.0.1` |
| `FLIPSTER_DATA_PORT` | N | publisher ZMQ PUB 포트 | `7000` |
| `IPC_SOCKET_PATH` | N | IPC Unix socket 경로 | `/tmp/flipster_data_subscriber.sock` |
| `RUST_LOG` | N | 로그 레벨 | `info` |

## 빌드 및 실행

```bash
cargo build --release
./target/release/data_subscriber
```

원격 publisher에 연결하는 경우:
```bash
DATA_SERVER_IP=192.168.1.100 ./target/release/data_subscriber
```

## IPC 프로토콜

### 클라이언트 등록

Unix socket 연결 후 첫 번째 줄로 등록 메시지를 전송:

```
REGISTER:<process_id>:<symbol1>,<symbol2>,...\n
```

- `process_id`: 클라이언트 식별자 (임의 문자열)
- 심볼 리스트가 비어 있으면 전체 심볼 구독

예시:
```
REGISTER:my_strategy:BTC-USDT-PERP,ETH-USDT-PERP\n   # 특정 심볼만
REGISTER:questdb_uploader:\n                            # 전체 심볼
```

### 메시지 프레이밍

등록 후 subscriber가 클라이언트에 메시지를 push:

```
[4B topic_len LE][topic bytes][4B payload_len LE][payload bytes]
```

- `topic_len`: topic 문자열 길이 (u32, little-endian)
- `topic`: 예: `flipster_bookticker_BTC-USDT-PERP`
- `payload_len`: payload 크기 (u32, little-endian) = 96
- `payload`: 96 bytes FlipsterBookTicker C-struct

### 클라이언트 구현 예시 (Rust pseudo-code)

```rust
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};

let mut stream = UnixStream::connect("/tmp/flipster_data_subscriber.sock").unwrap();

// 등록
stream.write_all(b"REGISTER:my_app:BTC-USDT-PERP\n").unwrap();

// 수신 루프
loop {
    let mut len_buf = [0u8; 4];

    // topic
    stream.read_exact(&mut len_buf).unwrap();
    let topic_len = u32::from_le_bytes(len_buf) as usize;
    let mut topic = vec![0u8; topic_len];
    stream.read_exact(&mut topic).unwrap();

    // payload
    stream.read_exact(&mut len_buf).unwrap();
    let payload_len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).unwrap();

    // 역직렬화 (little-endian)
    let bid = f64::from_le_bytes(payload[32..40].try_into().unwrap());
    let ask = f64::from_le_bytes(payload[40..48].try_into().unwrap());
    let server_ts = i64::from_le_bytes(payload[72..80].try_into().unwrap());
    println!("bid={bid} ask={ask} ts={server_ts}");
}
```

## ZMQ 소켓 설정

- `rcvhwm`: 100,000 (receive high-water mark)
- `rcvbuf`: 4 MB (OS-level receive buffer)
- 구독 필터: `flipster_bookticker_` (prefix match)
- Poll timeout: 100ms (Ctrl-C 체크용)
- Non-blocking drain: `DONTWAIT`로 가용한 모든 메시지 수신

## IPC 서버 아키텍처

```
Main thread:
  ZMQ SUB recv -> stamp timestamp -> broadcast to clients

Listener thread:
  UnixListener::accept() -> spawn per-client handler

Per-client handler thread:
  read registration -> create mpsc channel -> writer loop
```

- 각 클라이언트는 독립된 `mpsc::sync_channel(8192)` + writer thread를 가짐
- 느린 클라이언트의 채널이 가득 차면 해당 클라이언트만 drop (다른 클라이언트에 영향 없음)
- 연결 끊김 시 다음 broadcast에서 감지 후 제거

## 모니터링 (5초 간격 로그)

```
received=12345 forwarded=12345 clients=2 pub->sub: p50=15us p99=45us max=120us
```

- `received`: ZMQ에서 수신한 총 메시지 수
- `forwarded`: IPC 클라이언트에 전달된 메시지 수
- `clients`: 현재 연결된 IPC 클라이언트 수
- `pub->sub`: publisher -> subscriber latency (100개마다 샘플링)

## 프로젝트 구조

```
src/
  main.rs          # 엔트리포인트, ZMQ SUB 루프, latency 모니터링
  protocol.rs      # FlipsterBookTicker 바이너리 포맷 정의 + 역직렬화
  ipc_server.rs    # IPC Unix socket 서버, 클라이언트 등록/배포
```

## systemd 주의사항

systemd로 실행할 경우 `PrivateTmp=yes`가 기본 설정이면 `/tmp`가 서비스별로 격리됩니다.
IPC 클라이언트와 동일한 `/tmp`를 사용하려면:

1. `PrivateTmp=no` 설정, 또는
2. `IPC_SOCKET_PATH`를 `/tmp` 외부 경로로 지정 (예: `/var/run/hft/`)
