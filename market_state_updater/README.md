# market_state_updater

QuestDB 의 bookticker 테이블을 주기적으로 집계해서 **Redis 에 시장 상태 blob** 을 쓰는 데몬.
`gate_hft` 의 `update_market_state.py` 를 모듈화한 별도 Python 프로젝트.

소비자는 Rust 쪽 `market_watcher` (전략 컴포넌트). Redis blob shape 가 Rust `MarketGapState` 와 호환돼야 함 → blob 키 이름 변경 시 Rust 쪽도 같이 바꿔야 함.

## 5가지 Job

| job | Redis key | 의미 |
|---|---|---|
| `gap` | `{prefix}:{base}:{quote}:{window}` | base 거래소 mid vs quote 거래소 mid 의 평균 gap |
| `spread_pair` | `{prefix}:gate_gate_web_spreads:{window}` | gate_bookticker / gate_webbookticker 의 평균 (ask-bid)/mid |
| `gate_web_gap` | `{prefix}:gate_gate_web_mid_gap:{window}` | gate_bookticker mid vs gate_webbookticker mid 의 평균 gap |
| `price_change` | `{price_change_prefix}:{source}:{window}` | source 거래소의 (last_mid - first_mid) / first_mid |
| `price_change_gap_corr` | `{prefix}:price_change_gap_corr:gate_web:{quote}:{window}` | corr( gate_web mid 의 1-step return, gate_web↔quote mid gap ) per symbol |

윈도우 (분): gap / spread / gate_web_gap / corr 은 1, 5, 10, 30, 60, 240, 720. price_change 는 1, 5, 15, 30, 60, 240, 1440.
짧은 윈도우는 매 cycle, 긴 윈도우는 N cycle 마다 1번 (`WINDOW_PERIOD` / `PRICE_CHANGE_PERIOD` 참조).

**`price_change_gap_corr` 작동 방식**: window 별 X return interval (디폴트 dict, env override 가능):

| window | return interval | sample 수 |
|---|---|---|
| 1m | 1s | ~60 |
| 5m | 5s | ~60 |
| 10m | 5s | ~120 |
| 30m | 10s | ~180 |
| 60m | 30s | ~120 |
| 240m | 60s | ~240 |
| 720m | 300s | ~144 |

QuestDB 가 `corr()` 미지원이라 SQL 합성 (`(E[XY]-E[X]E[Y])/(σx·σy)`). LAG 미지원이라 prev mid 는 `dateadd('s', step, timestamp)` 으로 self-shift 한 view 와 JOIN. `n < 30` 또는 stddev `< 1e-10` 인 심볼은 SQL 단계에서 drop, `|corr| > 1.001` 인 numerical artifact 도 parser 에서 drop.

## 구조

```
market_state_updater/
├── src/market_state_updater/
│   ├── main.py              # CLI, 스케줄러 루프
│   ├── config.py            # AppConfig (CLI + env)
│   ├── scheduler.py         # Schedule dataclass + run_cycle
│   ├── questdb.py           # URL/auth 파싱 + exec
│   ├── questdb_http.py      # /exec GET/POST 폴백 (vendored)
│   └── jobs/
│       ├── common.py        # 윈도우/period 테이블, 공용 헬퍼
│       ├── gap.py           # base vs quote
│       ├── spread_pair.py   # gate vs gate_web spreads
│       ├── gate_web_gap.py  # gate vs gate_web mid gap
│       └── price_change.py  # 윈도우 내 mid 변화율
└── tests/                   # unit + integration (env 있을 때만)
```

`main.build_schedules()` 가 활성 job 들을 `list[Schedule]` 로 평면화 → `scheduler.run_cycle()` 이 `run_count` 보고 due 한 것만 호출. 새 job 추가는 `jobs/` 에 모듈 만들고 `build_schedules()` 에 한 줄 추가하면 끝.

## 실행

```bash
cp .env.example .env       # 값 채우기
uv sync --extra dev

# 한 cycle 만 (검증/CI)
uv run market-state-updater --once

# 운영 권장: fast / slow 두 데몬으로 분리
uv run market-state-updater --windows fast --interval 30    # ~8s/cycle, 1m·5m 윈도우
uv run market-state-updater --windows slow --interval 600   # ~40s/cycle, 10m+ 윈도우

# 단일 프로세스 모드 (개발/CI)
uv run market-state-updater --windows all --interval 60
```

**왜 fast/slow 분리가 권장인가**: `--windows all` 의 cycle 직렬 실행 시간이 ~64초여서 1m 윈도우의 정밀도가 흐려짐 (cycle 도는 동안 1분 지남). fast 만 분리하면 8초 cycle 이라 30초 interval 로 1m 윈도우가 의미 있게 갱신됨. slow 는 어차피 10분+ 윈도우라 10분 interval 로 충분.

CLI 옵션은 모두 env 로도 설정 가능 (`.env.example` 참조).

## 운영 모니터링

각 모드는 cycle 끝마다 **heartbeat 키** 에 결과를 JSON 으로 SET:

```
gate_hft:_meta:market_state_updater:fast
gate_hft:_meta:market_state_updater:slow
```

```jsonc
{"mode": "fast", "run_count": 42, "last_cycle_ms": 1777..., "due": 12, "ok": 12, "duration_ms": 8175}
```

외부 watchdog 은 `last_cycle_ms` 의 staleness 만 보면 됨.

**Telegram 알림** (옵션): `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` 둘 다 env 에 있으면 활성. cycle 이 `ALERT_AFTER_CONSECUTIVE_FAILURES` (default 5) 회 연속 완전 실패 (due > 0 AND ok == 0) 또는 예외로 죽으면 1회 알림. 한 cycle 이라도 성공하면 streak 리셋 + 복구 알림.

## 환경 변수

| key | default | 설명 |
|---|---|---|
| `QUESTDB_URL` | `http://localhost:9000` | `http://user:pass@host:port` 형태로 인증 가능 |
| `QUESTDB_QUERY_TIMEOUT_SECS` | 120 | |
| `REDIS_URL` | `redis://localhost:6379` | |
| `UPDATE_INTERVAL_SECS` | 10 | cycle 간 sleep |
| `WINDOW_MODE` | `all` | `fast` / `slow` / `all`. CLI `--windows` 와 동등 |
| `HEARTBEAT_REDIS_PREFIX` | `gate_hft:_meta:market_state_updater` | heartbeat 키 prefix |
| `TELEGRAM_BOT_TOKEN` | (none) | 둘 다 set 일 때만 알림 활성 |
| `TELEGRAM_CHAT_ID` | (none) | |
| `ALERT_AFTER_CONSECUTIVE_FAILURES` | 5 | N 회 연속 cycle failure 시 1회 알림 |
| `MARKET_GAP_REDIS_PREFIX` | `gate_hft:market_gap` | gap 계열 prefix |
| `MARKET_GAP_BASE` | `gate` | gap base 거래소 |
| `MARKET_GAP_QUOTE_EXCHANGES` | `binance` | comma-sep |
| `MARKET_GAP_INCLUDE_GATE_WEB` | 1 | gate_web 도 base 로 추가 |
| `MARKET_GAP_INCLUDE_GATE_SPREAD_PAIR` | 1 | spread_pair job 활성화 |
| `MARKET_GAP_INCLUDE_GATE_GATE_WEB_GAP` | 1 | gate_web_gap job 활성화 |
| `MARKET_GAP_INCLUDE_PRICE_CHANGE` | 1 | price_change job 활성화 |
| `MARKET_GAP_INCLUDE_PRICE_CHANGE_GAP_CORR` | 1 | corr job 활성화 |
| `CORR_QUOTE_EXCHANGES` | `binance` | corr 의 quote 후보 (현재 binance only 권장) |
| `CORR_RETURN_SECONDS_OVERRIDES` | (none) | `1:1,5:5,60:60` 형식. 윈도우별 X return interval override |
| `MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES` | 10 | 1m/5m 윈도우의 FILL(PREV) lookback |
| `PRICE_CHANGE_REDIS_PREFIX` | `gate_hft:price_change` | |
| `PRICE_CHANGE_SOURCES` | `gate,binance` | comma-sep |

## SQL 동작

- **1m / 5m 윈도우**: `SAMPLE BY 100T` (1m) / `1s` (5m) + `FILL(PREV)` 로 sparse 심볼의 stale price forward-fill. lookback 은 `MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES` (≥ window).
- **10m ~ 120m**: `SAMPLE BY 1s`, FILL 없음.
- **240m, 720m, 1440m**: `SAMPLE BY 5s` (쿼리 비용 ↓).
- gap 은 `JOIN ON symbol AND timestamp` 라서 두 feed 가 같은 시점에 모두 있어야 집계 대상.
- `|avg_mid_gap| ≥ 0.1` 인 심볼은 outlier 로 제외 (Rust `market_watcher.filter_valid_gaps` 와 동일).

## Blob shape (Rust 호환)

```jsonc
// {prefix}:{base}:{quote}:{window}
{
  "base_exchange": "gate",
  "quote_exchange": "binance",
  "window_minutes": 10,
  "updated_at_ms": 1234567890123,
  "avg_mid_gap_by_symbol": {"BTC_USDT": 0.001, ...},
  "avg_spread_by_symbol":  {"BTC_USDT": 0.0002, ...}  // base 거래소만
}

// {prefix}:gate_gate_web_spreads:{window}
{
  "window_minutes": 10, "updated_at_ms": ...,
  "avg_spread_gate_bookticker_by_symbol":    {...},
  "avg_spread_gate_webbookticker_by_symbol": {...}
}

// {prefix}:gate_gate_web_mid_gap:{window}
{
  "window_minutes": 10, "updated_at_ms": ...,
  "avg_mid_gap_by_symbol": {...}
}

// {price_change_prefix}:{source}:{window}
{
  "source": "gate", "window_minutes": 5, "updated_at_ms": ...,
  "price_change_by_symbol": {"BTC_USDT": -0.0012, ...}
}

// {prefix}:price_change_gap_corr:gate_web:{quote}:{window}
{
  "base_exchange": "gate_web",
  "quote_exchange": "binance",
  "window_minutes": 10,
  "return_seconds": 5,
  "updated_at_ms": ...,
  "corr_by_symbol":      {"SFP_USDT": 0.83, "PAXG_USDT": -0.56, ...},
  "n_samples_by_symbol": {"SFP_USDT": 120, ...}    // 신뢰도 가늠용
}
```

## 테스트

```bash
uv run pytest -q              # unit 만 실행 (Redis/QuestDB 없어도 됨)
uv run pytest tests/test_integration.py -v   # 실데이터 통합 (env 필요, 없으면 skip)
```

- `tests/test_query_builders.py` — 윈도우별 SQL 핵심 키워드 (FILL(PREV), SAMPLE BY, 테이블명) 검증.
- `tests/test_parsers.py` — QuestDB JSON payload → dict 파싱.
- `tests/test_scheduler.py` — `run_cycle` 의 period skip 로직.
- `tests/test_config.py` — env/CLI 머지.
- `tests/test_integration.py` — 실 QuestDB → 실 Redis 1cycle blob shape.
