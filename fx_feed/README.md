# fx_feed

FX bookticker 실시간 수집기. 여러 소스(Binance / dxFeed / Databento)에서 받아 **Gate FX 심볼 포맷**으로 정규화 후 **QuestDB**에 적재.

1차 목적: Gate HFT 전략의 "reference exchange" 슬롯을 FX로 확장하기 전에, **소스별 latency와 spread를 눈으로 확인**해서 먹을거 있는지 판단.

## 왜 필요한가

Gate HFT 전략은 reference 거래소(지금은 Binance)의 bookticker를 시그널로 쓰는데, 이걸 FX 심볼로 돌리려면
reference 쪽도 FX 데이터가 있어야 함. 후보:

1. **Binance** — 가장 나이브. FX-adjacent stablecoin 쌍만 있음 (`EURUSDT`, `GBPUSDT`, `AUDUSDT` 등). 진짜 FX majors(EUR/USD) 없음.
2. **dxFeed** — 리테일 FX 브로커 집계 피드. DXLink WebSocket, 토큰 인증.
3. **Databento** — 전문 데이터 벤더. `Live` 스트리밍, API key 필요.

전부 같은 테이블(`fx_bookticker`)에 `source` 컬럼 붙여서 넣고 QuestDB에서 심볼×소스별 delay/spread 비교.

## 구조

```
fx_feed/
├── src/fx_feed/
│   ├── types.py              # FxTick dataclass
│   ├── questdb_sink.py       # ILP writer (delay_ms 포함)
│   ├── symbol_map.py         # source symbol → Gate FX symbol 정규화
│   ├── main.py               # CLI entrypoint (--source ...)
│   └── sources/
│       ├── base.py           # FxSource Protocol
│       ├── binance.py        # WS bookticker (public, 무인증)
│       ├── dxfeed.py         # DXLink WS (토큰)
│       └── databento.py      # databento.Live (API key)
├── scripts/
│   └── inspect.sql           # QuestDB 분석 쿼리
└── config.example.toml
```

## 빠른 실행

```bash
cd fx_feed
cp config.example.toml config.toml
cp .env.example .env   # DXFEED_TOKEN, DATABENTO_API_KEY 채우기
uv sync --extra databento

# 소스 하나씩 실행 (각각 별도 프로세스)
PYTHONPATH=src uv run python -m fx_feed.main --source binance   config.toml
PYTHONPATH=src uv run python -m fx_feed.main --source dxfeed    config.toml
PYTHONPATH=src uv run python -m fx_feed.main --source databento config.toml
```

## QuestDB 테이블: `fx_bookticker`

| 컬럼 | 타입 | 설명 |
|---|---|---|
| `source` | SYMBOL | `binance` / `dxfeed` / `databento` |
| `symbol` | SYMBOL | Gate FX 포맷 (예: `EUR_USD`) |
| `raw_symbol` | SYMBOL | 원본 포맷 (디버깅용) |
| `bid_price` | DOUBLE | |
| `ask_price` | DOUBLE | |
| `bid_size` | DOUBLE | (없으면 NaN) |
| `ask_size` | DOUBLE | (없으면 NaN) |
| `source_ts` | TIMESTAMP | 소스 이벤트 타임스탬프 |
| `delay_ms` | DOUBLE | recv_ns - source_ns (ms) |
| `timestamp` | TIMESTAMP | 로컬 수신 시각 (designated) |

`scripts/inspect.sql`에 소스별 spread / p50·p99 latency 비교 쿼리.

## 심볼 매핑

`config.toml`의 `[symbols.<source>]` 섹션에 `raw_symbol = "gate_fx_symbol"` 형태로 명시. Gate FX 정확한 포맷이 확정되기 전엔 일단 `EUR_USD`, `GBP_USD`, `USD_JPY` 같은 placeholder로 통일. Haechan에게 Gate FX 심볼 리스트 받으면 config만 고치면 됨.

## 로드맵

- [x] 스캐폴딩 + Binance 스트리밍
- [ ] dxFeed DXLink 실연동 (토큰 발급 필요)
- [ ] Databento Live 실연동 (API key 필요, dataset/schema 확정 필요)
- [ ] QuestDB 적재 검증
- [ ] 소스별 latency/spread 비교 대시보드
- [ ] 가장 빠른 소스 확정 후 `feed/data_publisher` 포맷으로 ZMQ 래핑 (전략이 바로 꽂히게)
