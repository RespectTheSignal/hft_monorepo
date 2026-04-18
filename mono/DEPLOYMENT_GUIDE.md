# HFT Monorepo — Production 배포 가이드

> **대상 독자**: 운영 인력 (DevOps / SRE / 퀀트 엔지니어)
>
> **전제 조건**: Phase 3 코드 완성 (`mono-monorepo-초기화-260415-sigma13-K` 브랜치, commit `f40f731`).
> 본 문서는 빌드된 바이너리를 실 서버에 올리고 운영하기까지의 전 과정을 다룬다.

---

## 목차

1. [아키텍처 개요](#1-아키텍처-개요)
2. [사전 요구사항](#2-사전-요구사항)
3. [빌드](#3-빌드)
4. [환경 설정 파일 준비](#4-환경-설정-파일-준비)
5. [서비스별 배포 순서](#5-서비스별-배포-순서)
6. [모니터링 스택 배포](#6-모니터링-스택-배포)
7. [멀티 서브계정 설정](#7-멀티-서브계정-설정)
8. [Strategy main.rs 멀티계정 Wiring (후속 개발 필요)](#8-strategy-mainrs-멀티계정-wiring)
9. [운영 CLI 도구](#9-운영-cli-도구)
10. [Futures Collector (시장 데이터 아카이브)](#10-futures-collector)
11. [트러블슈팅](#11-트러블슈팅)
12. [환경변수 전체 레퍼런스](#12-환경변수-전체-레퍼런스)

---

## 1. 아키텍처 개요

```
┌─────────────────────────────────── 물리 호스트 (Proxmox / QEMU) ───────────────────────────────────┐
│                                                                                                     │
│  ┌──────────── Infra VM ────────────┐    ivshmem     ┌──────── Strategy VM 0 ────────┐              │
│  │  hft-publisher      (port 9100)  │◄══════════════►│  hft-strategy        (9102)   │              │
│  │  hft-order-gateway  (port 9101)  │    PCI BAR     │  hft-futures-collector        │              │
│  │                                  │                │                                │              │
│  └──────────────────────────────────┘                └────────────────────────────────┘              │
│                                                                                                     │
│  ┌──────── Monitoring (Docker) ─────┐                ┌──────── Strategy VM 1 ────────┐              │
│  │  Prometheus          (9090)      │                │  hft-strategy        (9103)   │              │
│  │  Grafana             (3000)      │                │  ...                          │              │
│  │  QuestDB             (9009/8812) │                └────────────────────────────────┘              │
│  └──────────────────────────────────┘                                                               │
│                                                                                                     │
│  ┌──────── 독립 서비스 ─────────────┐                                                               │
│  │  hft-monitoring-agent            │                                                               │
│  │  (Telegram alert, 5s scrape)     │                                                               │
│  └──────────────────────────────────┘                                                               │
└─────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**데이터 흐름**:

1. `publisher` — 거래소 WebSocket 으로 bookticker/trade 수신 → SHM(SharedRegion) 에 기록
2. `strategy` — SHM 에서 시세를 읽고, 전략 로직 실행 → 주문 요청을 SHM order ring 또는 ZMQ 로 전송
3. `order-gateway` — 주문 수신 → Gate REST/WS 로 실주문 집행 → 결과를 ZMQ PUSH 로 strategy 에 반환
4. `monitoring-agent` — 3 서비스의 `/metrics` 스크래핑 → 14개 규칙 기반 Telegram 알림
5. `futures-collector` — subscriber 경유 시세 → Parquet Hive 파티션 아카이브

---

## 2. 사전 요구사항

### 2.1 호스트 준비

```bash
# deploy/scripts/host_bootstrap.sh 실행
# 점검 항목:
#   - grub cmdline: hugepages, isolcpus
#   - /dev/hugepages 마운트
#   - CPU governor = performance
#   - KVM 모듈 로드
#   - THP (Transparent Huge Pages) = never

cd mono/deploy/scripts
chmod +x host_bootstrap.sh
./host_bootstrap.sh           # report-only 모드 (기본)
./host_bootstrap.sh --apply   # runtime 변경 적용
```

exit code: 0=통과, 1=경고(운영 가능하나 권장 변경), 2=치명적(재부팅 필요).

### 2.2 소프트웨어 요구사항

| 컴포넌트 | 최소 버전 | 용도 |
|----------|-----------|------|
| Rust toolchain | 1.75+ | 빌드 |
| Docker + docker-compose | 24.0+ / 2.20+ | 모니터링 스택 |
| QEMU/KVM | 8.0+ | VM 관리 (ivshmem 지원) |
| Redis | 7.0+ | 전략 상태 저장 (선택) |
| QuestDB | 7.3+ | 레이턴시 시계열 DB (선택) |

### 2.3 네트워크 포트

| 포트 | 서비스 | 프로토콜 |
|------|--------|----------|
| 9100 | publisher `/metrics` + `/health` | HTTP |
| 9101 | order-gateway `/metrics` + `/health` | HTTP |
| 9102+ | strategy `/metrics` + `/health` | HTTP |
| 9090 | Prometheus | HTTP |
| 3000 | Grafana | HTTP |
| 9009 | QuestDB ILP | TCP |
| 8812 | QuestDB PG wire | TCP |
| 6379 | Redis | TCP |

---

## 3. 빌드

```bash
cd hft_monorepo/mono

# Release 빌드 (전체 workspace)
cargo build --release --workspace

# 바이너리 위치: target/release/
#   hft-publisher
#   hft-order-gateway
#   hft-strategy
#   hft-monitoring-agent
#   hft-futures-collector
#   hft-healthcheck
#   hft-status-check
#   hft-close-positions
#   hft-latency-probe
#   hft-questdb-export

# 배포 대상 서버로 복사
scp target/release/hft-{publisher,order-gateway,strategy,monitoring-agent,futures-collector} user@infra-vm:/usr/local/bin/
scp target/release/hft-{healthcheck,status-check,close-positions} user@infra-vm:/usr/local/bin/
```

---

## 4. 환경 설정 파일 준비

### 4.1 secrets.env (API 키 + 토큰)

```bash
# /etc/hft/secrets.env  (chmod 600, root 소유)
# ──────────────────────────────────────────

# Gate.io 계정 (단일 계정 모드)
GATE_API_KEY=your_gate_api_key
GATE_API_SECRET=your_gate_api_secret

# 다른 거래소 (order-gateway 에서 사용, 현재 Gate 만 활성)
# HFT_BINANCE_API_KEY=
# HFT_BINANCE_API_SECRET=
# HFT_OKX_API_KEY=
# HFT_OKX_API_SECRET=
# HFT_OKX_PASSPHRASE=

# Telegram 알림 (monitoring-agent)
TELEGRAM_BOT_TOKEN=your_bot_token
TELEGRAM_CHAT_ID=your_chat_id

# Redis (선택 — 미설정 시 상태 저장 비활성, 전략은 정상 작동)
# REDIS_URL=redis://127.0.0.1:6379
```

> **보안 주의**: `secrets.env` 는 절대 git 에 커밋하지 않는다. `chmod 600` + systemd `EnvironmentFile=` 로만 주입.

### 4.2 SHM 레이아웃 파라미터 (publisher ↔ strategy 반드시 동일)

이 값들이 publisher 와 strategy 사이에 **한 바이트라도 다르면 torn read / 데이터 오염** 발생한다.

```bash
# 공통 SHM 레이아웃 (모든 서비스에 동일하게 설정)
HFT_SHM__N_MAX=16
HFT_SHM__QUOTE_SLOTS=8192
HFT_SHM__TRADE_RING_CAPACITY=65536
HFT_SHM__ORDER_RING_CAPACITY=4096
HFT_SHM__SYMBOL_TABLE_CAPACITY=4096
```

| 파라미터 | 의미 | 기본값 | 조정 기준 |
|----------|------|--------|-----------|
| N_MAX | 최대 strategy VM 수 | 16 | VM 추가 시 확장 |
| QUOTE_SLOTS | 심볼당 quote 슬롯 수 | 8192 | 심볼 수 × 2 이상 |
| TRADE_RING_CAPACITY | trade ring 용량 | 65536 | 2의 거듭제곱 |
| ORDER_RING_CAPACITY | 주문 ring 용량 | 4096 | 2의 거듭제곱 |

### 4.3 서비스별 환경변수 파일

각 서비스의 환경변수는 systemd unit 파일의 `Environment=` 또는 `EnvironmentFile=` 으로 주입한다. 템플릿은 `deploy/systemd/hft-*.service` 에 있다.

---

## 5. 서비스별 배포 순서

**반드시 아래 순서를 지킨다.** publisher 가 SHM 을 생성해야 다른 서비스가 attach 가능.

### 5.1 Step 1 — Publisher (Infra VM)

```bash
# systemd unit 설치
sudo cp deploy/systemd/hft-publisher.service /etc/systemd/system/
sudo systemctl daemon-reload

# 환경변수 확인 후 시작
sudo systemctl start hft-publisher
sudo systemctl status hft-publisher

# health check
curl http://localhost:9100/health
# 기대 응답: {"status":"ok","service":"publisher"}
```

주요 환경변수:
```bash
HFT_SHM__BACKING=pci_bar              # 프로덕션: pci_bar (ivshmem)
HFT_SHM__PCI_BAR_PATH=/sys/bus/pci/devices/0000:00:04.0/resource2
HFT_SHM__ROLE=publisher
HFT_SHM__VM_ID=0
HFT_PUBLISHER_READINESS_PORT=8188
HFT_TELEMETRY__PROM_PORT=9100
HFT_SERVICE_NAME=publisher
```

### 5.2 Step 2 — Order Gateway (Infra VM)

publisher 가 정상 기동된 후 시작한다. systemd `Requires=hft-publisher.service` 로 의존성이 설정되어 있다.

```bash
sudo cp deploy/systemd/hft-order-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start hft-order-gateway

curl http://localhost:9101/health
```

주요 환경변수:
```bash
HFT_TELEMETRY__PROM_PORT=9101
HFT_SERVICE_NAME=order-gateway
# GATE_API_KEY / GATE_API_SECRET 는 secrets.env 에서 주입
```

### 5.3 Step 3 — Strategy (Strategy VM)

```bash
# Strategy VM 에서
sudo cp deploy/systemd/hft-strategy.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start hft-strategy

curl http://localhost:9102/health
```

주요 환경변수:
```bash
HFT_SHM__BACKING=pci_bar
HFT_SHM__PCI_BAR_PATH=/sys/bus/pci/devices/0000:00:04.0/resource2
HFT_SHM__ROLE=strategy
HFT_SHM__VM_ID=0                       # VM마다 고유 (0, 1, 2, ...)
HFT_STRATEGY_VARIANT=v8                # noop | v6 | v7 | v8 | close_v1 | mm_close | close_unhealthy
HFT_STRATEGY_LOGIN_NAME=sigma01
HFT_STRATEGY_SYMBOLS=BTC_USDT,ETH_USDT # 비어있으면 전체 심볼
HFT_LEVERAGE=50.0
HFT_TELEMETRY__PROM_PORT=9102
HFT_BALANCE_PUMP_MS=500
HFT_RATE_DECAY_MS=1000
# GATE_API_KEY / GATE_API_SECRET 는 secrets.env 에서 주입
# REDIS_URL 은 선택 (미설정 시 상태 저장 비활성)
```

### 5.4 로컬 개발 환경 (단일 호스트)

ivshmem 없이 `/dev/shm` 백킹으로 빠르게 테스트:

```bash
cd mono/deploy/tmux
chmod +x dev_layout.sh
./dev_layout.sh
# tmux 4분할: publisher | gateway | strategy-0 | strategy-1
```

`dev_layout.sh` 은 `HFT_SHM__BACKING=dev_shm` 으로 설정하여 VM 없이 단일 프로세스로 전체 파이프라인을 구동한다.

---

## 6. 모니터링 스택 배포

### 6.1 Prometheus + Grafana (Docker)

```bash
cd mono/deploy/monitoring

# scrape target 주소 편집 (실 환경에 맞게)
vi prometheus/prometheus.yml
# targets 를 실제 VM IP 로 변경:
#   - infra-vm:9100   (publisher)
#   - infra-vm:9101   (order-gateway)
#   - strategy-vm-0:9102  (strategy)

# 시작
docker-compose up -d

# 확인
curl http://localhost:9090/-/healthy    # Prometheus
curl http://localhost:3000/api/health   # Grafana (admin/admin)
```

Grafana 대시보드는 `grafana/dashboards/hft-pipeline.json` 에서 auto-provisioning 된다. 5개 행:
- Overview (서비스 up/down, active strategies)
- Latency (e2e, per-stage percentiles)
- SHM (ring utilization, quote freshness)
- Orders (submitted, accepted, rejected rates)
- Heartbeat (gateway liveness, age)

### 6.2 QuestDB (선택)

레이턴시 시계열 기록용. 별도 컨테이너 또는 bare-metal 로 운영.

```bash
docker run -d --name questdb \
  -p 9009:9009 -p 8812:8812 -p 9000:9000 \
  -v /data/questdb:/var/lib/questdb \
  questdb/questdb:7.3

# DDL 초기화 — questdb-export 바이너리가 자동으로 수행
# 수동 확인:
#   psql -h localhost -p 8812 -U admin -d qdb
#   \dt   → latency_stages 테이블 존재 확인
```

### 6.3 Monitoring Agent (Telegram 알림)

```bash
sudo cp deploy/systemd/hft-monitoring-agent.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start hft-monitoring-agent
```

주요 환경변수:
```bash
HFT_MONITORING__SCRAPE_INTERVAL_SECS=5
HFT_MONITORING__PUBLISHER_ADDR=http://infra-vm:9100
HFT_MONITORING__GATEWAY_ADDR=http://infra-vm:9101
HFT_MONITORING__STRATEGY_ADDR=http://strategy-vm-0:9102
# TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID 는 secrets.env 에서 주입
```

Telegram Bot 생성 방법:
1. Telegram 에서 `@BotFather` 대화 → `/newbot` → 봇 이름 지정 → 토큰 발급
2. 알림 받을 채팅방에 봇 초대 → 해당 chat ID 확인 (`https://api.telegram.org/bot<TOKEN>/getUpdates`)
3. `secrets.env` 에 `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID` 기입

14개 알림 규칙 (threshold/rate/counter):
- heartbeat_stale, service_down, no_strategies
- e2e_latency_high, shm_ring_full
- order_rejected_rate, supervisor_restart_count
- 등 (상세는 `crates/services/monitoring-agent/src/rules.rs` 참조)

---

## 7. 멀티 서브계정 설정

### 7.1 단일 계정 모드 (기본)

`GATE_API_KEY` / `GATE_API_SECRET` 환경변수만 설정하면 단일 계정으로 동작한다. `HFT_ACCOUNTS_FILE` 이 없으면 자동으로 이 모드.

### 7.2 멀티 계정 모드

`/etc/hft/accounts.json` 파일을 생성하고 `HFT_ACCOUNTS_FILE` 환경변수로 지정:

```json
{
  "accounts": [
    {
      "login_name": "sigma01",
      "api_key": "key_for_sigma01",
      "api_secret": "secret_for_sigma01",
      "symbols": ["BTC_USDT", "ETH_USDT"],
      "leverage": 50.0,
      "enabled": true
    },
    {
      "login_name": "sigma02",
      "api_key": "key_for_sigma02",
      "api_secret": "secret_for_sigma02",
      "symbols": ["SOL_USDT", "DOGE_USDT"],
      "leverage": 20.0,
      "enabled": true
    },
    {
      "login_name": "sigma03_disabled",
      "api_key": "...",
      "api_secret": "...",
      "symbols": [],
      "leverage": null,
      "enabled": false
    }
  ]
}
```

필드 설명:

| 필드 | 타입 | 필수 | 설명 |
|------|------|------|------|
| login_name | string | O | 계정 식별자 (중복 불가) |
| api_key | string | O | Gate API key |
| api_secret | string | O | Gate API secret |
| symbols | string[] | X | 이 계정이 운영할 심볼. 빈 배열 = 전체 |
| leverage | float / null | X | 계정별 레버리지 override. null = 글로벌 기본값 |
| enabled | bool | X | false 면 로딩하되 polling/trading 비활성 (기본 true) |

> **보안 주의**: `accounts.json` 에 API 키가 평문으로 들어간다. `chmod 600` 필수. 프로덕션에서는 Vault/SOPS 등 시크릿 매니저 연동 권장.

### 7.3 login_names range expansion

CLI 또는 환경변수에서 범위 지정 가능:

```bash
# 단일
HFT_LOGIN_NAMES=sigma01

# 쉼표 구분
HFT_LOGIN_NAMES=sigma01,sigma02,sigma03

# 범위 확장 (prefix + 숫자 suffix)
HFT_LOGIN_NAMES=sigma001~sigma010
# → sigma001, sigma002, ..., sigma010

# 혼합
HFT_LOGIN_NAMES=alpha,sigma001~sigma003,beta
# → alpha, sigma001, sigma002, sigma003, beta
```

범위 확장 규칙:
- prefix 가 동일해야 한다 (`sigma01~sigma05` OK, `alpha01~beta05` 에러)
- 숫자 suffix 의 zero-padding width 가 보존된다 (`acc008~acc012` → acc008, acc009, acc010, acc011, acc012)
- start > end 이면 에러

### 7.4 IP 감지 (서브계정 격리)

서브계정별로 다른 IP 를 사용해야 하는 경우 (Gate.io 제약), `hft_account::detect_local_ip()` 로 현재 프로세스의 egress IP 를 확인할 수 있다. 로그에 자동 출력되며, 수동 확인:

```bash
# strategy 바이너리 로그에서 확인
journalctl -u hft-strategy | grep "local IP"
```

---

## 8. Strategy main.rs 멀티계정 Wiring

> **현재 상태**: `hft-account` 크레이트는 완성되었으나, `strategy/src/main.rs` 는 아직 **단일 계정 경로** (`GATE_API_KEY` → 단일 `GateAccountClient` → 단일 `AccountPoller`) 만 사용한다.

### 8.1 현재 동작 (단일 계정)

```
GATE_API_KEY/SECRET
    → Credentials
        → GateAccountClient
            → AccountPoller (meta / positions / balance)
                → PositionOracleImpl
                    → Strategy hot loop
```

### 8.2 후속 개발: 멀티계정 통합

멀티계정 wiring 을 완성하려면 `strategy/src/main.rs` 의 `bring_up_full()` 에서 다음 변경이 필요하다:

```
기존:  gate_api_credentials() → 단일 Credentials
변경:  AccountManager::from_env() → Vec<AccountHandle>

기존:  maybe_spawn_gate_poller(meta, positions, balance) → 단일 PollerHandle
변경:  for handle in mgr.enabled_accounts() {
           AccountPoller::builder(handle.client.clone())
               .meta(per_account_meta)
               .positions(per_account_positions)
               .balance_slot(per_account_balance)
               .spawn()
       }

기존:  spawn_gate_user_stream(meta, positions, balance, control_tx, cancel) → 단일 JoinHandle
변경:  for handle in mgr.enabled_accounts() {
           spawn_gate_user_stream(handle.credentials, ...)
       }
```

핵심 설계 결정:
- **per-account 캐시 분리**: 각 계정이 독립 `PositionCache` / `BalanceSlot` 보유
- **AccountMembership 확장**: `(login_name, symbol) → bool` 2차원 멤버십 체크
- **주문 라우팅**: 심볼 → 어떤 계정의 OrderSender 로 보낼지 라우팅 테이블 필요
- **MarginManager 주기 스냅샷**: 전 계정 잔고를 주기적으로 수집하여 리스크 관리

이 작업이 완료되기 전까지는 **프로세스당 단일 계정** 으로 운영한다:
```bash
# VM 0: 계정 sigma01
HFT_STRATEGY_LOGIN_NAME=sigma01
GATE_API_KEY=key_for_sigma01
GATE_API_SECRET=secret_for_sigma01

# VM 1: 계정 sigma02
HFT_STRATEGY_LOGIN_NAME=sigma02
GATE_API_KEY=key_for_sigma02
GATE_API_SECRET=secret_for_sigma02
```

---

## 9. 운영 CLI 도구

### 9.1 healthcheck — 서비스 상태 점검

```bash
hft-healthcheck --services publisher:9100,gateway:9101,strategy:9102

# Gate API 실주문 테스트 (place + cancel)
hft-healthcheck --services publisher:9100 --gate-test-order
```

### 9.2 status-check — 파이프라인 통합 대시보드

```bash
hft-status-check

# JSON 출력 (스크립트 연동)
hft-status-check --json
```

출력 항목: 계정 잔고, 오픈 포지션, 레이턴시 p50/p99, 활성 알림, 서비스 상태.

### 9.3 close-positions — 긴급 전량 청산

```bash
# dry-run (실제 주문 없음, 청산 대상만 표시)
hft-close-positions --dry-run

# 실행 (확인 프롬프트 표시)
hft-close-positions

# 확인 프롬프트 없이 즉시 실행 (자동화 스크립트용)
hft-close-positions --yes
```

> **주의**: `--yes` 플래그는 확인 없이 즉시 전량 시장가 청산한다. 운영 중 사고 시에만 사용할 것.

---

## 10. Futures Collector

시장 데이터를 Parquet Hive 파티션 형식으로 아카이브한다.

```bash
sudo cp deploy/systemd/hft-futures-collector.service /etc/systemd/system/
sudo cp deploy/systemd/hft-futures-collector-rotate.service /etc/systemd/system/
sudo cp deploy/systemd/hft-futures-collector-rotate.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now hft-futures-collector
sudo systemctl enable --now hft-futures-collector-rotate.timer
```

저장 구조:
```
/data/hft/collected/
├── type=bookticker/
│   └── year=2026/month=04/day=18/hour=09/
│       └── data.parquet     (BookTickerRow: 9 cols, ZSTD lv3)
└── type=trade/
    └── year=2026/month=04/day=18/hour=09/
        └── data.parquet     (TradeRow: 10 cols, ZSTD lv3)
```

30일 초과 파일은 `hft-futures-collector-rotate.timer` 가 매일 자동 정리.

---

## 11. 트러블슈팅

### 11.1 SHM attach 실패

```
ERROR SharedRegion::open_view failed: ...
```

원인: publisher 미기동 상태에서 strategy 가 먼저 시작됨.
해결: `systemctl start hft-publisher` 후 strategy 재시작.

### 11.2 SHM 레이아웃 불일치

```
ERROR layout mismatch: expected N_MAX=16, got 8
```

원인: publisher 와 strategy 의 SHM 파라미터가 다름.
해결: 양쪽 `HFT_SHM__*` 환경변수를 동일하게 맞춘 후 양쪽 재시작. SHM 파일 삭제 필요할 수 있음.

### 11.3 Gate API 인증 실패

```
WARN Gate API credentials missing — skipping Gate account poller
```

원인: `GATE_API_KEY` / `GATE_API_SECRET` 미설정 또는 빈 문자열.
해결: `secrets.env` 확인. strategy 는 creds 없이도 noop 모드로 동작 가능.

### 11.4 Redis 연결 실패 (graceful degrade)

```
WARN Redis connection failed — operating without state persistence
```

정상 동작. `hft-state` 는 Redis 없이도 모든 기능이 동작한다. 다만 재시작 시 last_order 복원과 session 기록이 안 된다.

### 11.5 디스크 부족 (빌드 시)

```bash
# target/ 디렉터리가 10GB+ 차지
cargo clean --manifest-path mono/Cargo.toml
# 약 11-12GB 확보
```

### 11.6 Telegram 알림 안 옴

1. `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID` 확인
2. Bot 이 해당 chat 에 초대되었는지 확인
3. `journalctl -u hft-monitoring-agent` 에서 에러 로그 확인
4. 수동 API 테스트: `curl "https://api.telegram.org/bot<TOKEN>/sendMessage?chat_id=<ID>&text=test"`

---

## 12. 환경변수 전체 레퍼런스

### 공통

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_CONFIG_DIR` | `/etc/hft` | 설정 파일 디렉터리 |
| `HFT_LOG` | `info` | tracing EnvFilter (예: `info,hft=debug`) |
| `HFT_SERVICE_NAME` | — | 서비스 식별자 (텔레메트리 라벨) |

### SHM (publisher ↔ strategy 동일 필수)

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_SHM__BACKING` | `dev_shm` | `dev_shm` / `hugetlbfs` / `pci_bar` |
| `HFT_SHM__SHARED_PATH` | — | dev_shm/hugetlbfs 경로 |
| `HFT_SHM__PCI_BAR_PATH` | — | ivshmem PCI BAR2 경로 |
| `HFT_SHM__N_MAX` | `16` | 최대 VM 수 |
| `HFT_SHM__QUOTE_SLOTS` | `8192` | quote 슬롯 수 |
| `HFT_SHM__TRADE_RING_CAPACITY` | `65536` | trade ring 용량 |
| `HFT_SHM__ORDER_RING_CAPACITY` | `4096` | order ring 용량 |
| `HFT_SHM__SYMBOL_TABLE_CAPACITY` | `4096` | symbol table 용량 |
| `HFT_SHM__ROLE` | — | `publisher` / `strategy` |
| `HFT_SHM__VM_ID` | `0` | VM 고유 ID (strategy 마다 다름) |

### Publisher

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_PUBLISHER_READINESS_PORT` | `8188` | readiness probe 포트 |

### Strategy

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_STRATEGY_VARIANT` | `noop` | `noop`/`v6`/`v7`/`v8`/`close_v1`/`mm_close`/`close_unhealthy` |
| `HFT_STRATEGY_LOGIN_NAME` | `default` | 계정 식별자 |
| `HFT_STRATEGY_SYMBOLS` | (전체) | 쉼표 구분 심볼 리스트 |
| `HFT_STRATEGY_ACCOUNT_MODE` | `shared` | `shared` / `isolated` |
| `HFT_LEVERAGE` | `50.0` | 레버리지 배수 |
| `HFT_BALANCE_PUMP_MS` | `500` | 잔고 pump 주기 (ms) |
| `HFT_RATE_DECAY_MS` | `1000` | rate tracker decay 주기 (ms) |

### 멀티 계정

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_ACCOUNTS_FILE` | (없음) | 멀티계정 JSON 경로. 미설정 = 단일계정 |
| `HFT_LOGIN_NAMES` | (없음) | range expansion 입력 (향후 CLI 연동) |

### Credentials

| 변수 | 설명 |
|------|------|
| `GATE_API_KEY` | Gate.io API key |
| `GATE_API_SECRET` | Gate.io API secret |
| `TELEGRAM_BOT_TOKEN` | Telegram bot 토큰 |
| `TELEGRAM_CHAT_ID` | Telegram chat ID |
| `REDIS_URL` | Redis 연결 문자열 (선택) |

### Telemetry

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_TELEMETRY__PROM_PORT` | `9100` | Prometheus metrics + health 포트 |
| `HFT_TELEMETRY__OTLP_ENDPOINT` | (없음) | OTLP exporter 엔드포인트 (선택) |
| `HFT_TELEMETRY__STDOUT_JSON` | `false` | JSON 형식 로그 출력 |

### Monitoring Agent

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_MONITORING__SCRAPE_INTERVAL_SECS` | `5` | 메트릭 스크래핑 주기 |
| `HFT_MONITORING__PUBLISHER_ADDR` | — | publisher 메트릭 주소 |
| `HFT_MONITORING__GATEWAY_ADDR` | — | gateway 메트릭 주소 |
| `HFT_MONITORING__STRATEGY_ADDR` | — | strategy 메트릭 주소 |

### Futures Collector

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `HFT_COLLECTOR__OUTPUT_DIR` | `/data/hft/collected` | Parquet 출력 경로 |
| `HFT_COLLECTOR__COMPRESSION` | `zstd` | 압축 알고리즘 |
| `HFT_COLLECTOR__COMPRESSION_LEVEL` | `3` | 압축 레벨 |
| `HFT_COLLECTOR__ROW_GROUP_SIZE` | `100000` | Parquet row group 크기 |
| `HFT_COLLECTOR__RETENTION_DAYS` | `30` | 보존 기간 (rotate timer) |

---

## 부록: 운영 체크리스트

### 최초 배포

- [ ] `host_bootstrap.sh` 통과 (exit code 0)
- [ ] `/etc/hft/secrets.env` 생성 (chmod 600)
- [ ] SHM 파라미터 publisher/strategy 동일 확인
- [ ] `cargo build --release --workspace` 성공
- [ ] 바이너리 배포 완료 (`/usr/local/bin/hft-*`)
- [ ] systemd unit 파일 설치 + `daemon-reload`
- [ ] publisher → gateway → strategy 순서대로 기동
- [ ] 각 서비스 `/health` 응답 확인
- [ ] Prometheus + Grafana docker-compose up
- [ ] Grafana 대시보드에서 메트릭 수신 확인
- [ ] Telegram bot 생성 + monitoring-agent 기동
- [ ] Telegram 테스트 알림 수신 확인
- [ ] `hft-healthcheck` 전체 통과
- [ ] `hft-status-check` 출력 정상 확인

### 일상 운영

- [ ] `hft-status-check` 주기적 실행 (또는 Grafana 상시 모니터링)
- [ ] Telegram 알림 응답 (heartbeat_stale, service_down 등)
- [ ] 디스크 사용량 모니터링 (Parquet, QuestDB, target/)
- [ ] secrets.env 키 rotation (분기별 권장)

### 긴급 상황

- [ ] `hft-close-positions --dry-run` 으로 포지션 확인
- [ ] `hft-close-positions` 으로 전량 청산
- [ ] `systemctl stop hft-strategy` 로 전략 중단
- [ ] Telegram + Grafana 에서 상황 확인
