<!-- team-reporter: enabled -->

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# 의존성 설치 (uv 사용, src-layout)
uv sync --extra dev

# 한 cycle 만 (검증/CI)
uv run market-state-updater --once

# 운영 권장: fast / slow 두 데몬 분리
uv run market-state-updater --windows fast --interval 30    # ~8s/cycle  (1m, 5m)
uv run market-state-updater --windows slow --interval 600   # ~40s/cycle (10m+)

# 단일 프로세스로 전부 (개발/CI)
uv run market-state-updater --windows all --interval 60

# 전체 테스트 (unit + integration, integration 은 .env 의 QUESTDB_URL/REDIS_URL 살아있을 때만)
uv run pytest -q

# 단일 테스트 파일 / 함수
uv run pytest tests/test_query_builders.py -v
uv run pytest tests/test_parsers.py::test_gap_parse_full_payload -v

# unit 만 (외부 의존 0, integration 강제 skip)
uv run pytest -q --ignore=tests/test_integration.py

# 타입 체크
uv run mypy src/market_state_updater
```

CLI 옵션은 모두 env 로도 설정 가능 (`.env.example` 참조). `.env` 가 cwd 에 있으면 `dotenv` 가 자동 로드.

## Project Overview

QuestDB 에서 bookticker 테이블을 집계해 Redis 에 시장 상태 blob 을 쓰는 daemon. 소비자는 Rust 쪽 `market_watcher` (전략).

원본은 `gate_hft/update_market_state.py` (~960줄 단일 파일). 이걸 모노레포에 src-layout + 모듈 분리한 버전.

## 동작 방식

`main.build_schedules(cfg, redis)` 가 활성 job × 윈도우 × 거래소를 평면화해서 `list[Schedule]` 을 만듦.
각 `Schedule(name, period, run)` 의 `run` 은 `functools.partial` 로 미리 bound 된 closure.
`scheduler.run_cycle(schedules, run_count)` 가 `run_count % period == 0` 인 것만 직렬 호출.

새 job 추가 = `jobs/<name>.py` 작성 + `main.build_schedules()` 에 for 루프 한 블록 추가.

## Job 5종

- `jobs/gap.py` — base 거래소 mid vs quote 거래소 mid (Rust `MarketGapState` 와 호환)
- `jobs/spread_pair.py` — gate_bookticker / gate_webbookticker 의 (ask-bid)/mid 비교
- `jobs/gate_web_gap.py` — gate vs gate_web mid gap (단일 거래소 내 두 피드 차이)
- `jobs/price_change.py` — 윈도우 내 (last_mid - first_mid) / first_mid
- `jobs/price_change_gap_corr.py` — Pearson corr( gate_web 1-step return, gate_web↔quote gap ). QuestDB 미지원 함수 (`corr`, `LAG`) 우회: stddev/avg 합성 + dateadd self-shift JOIN. 윈도우별 X return interval 은 `corr_return_seconds()` 에서 결정 (`DEFAULT_CORR_RETURN_SECONDS` + env override). 신뢰도 안전장치: SQL 단계 `n >= MIN_SAMPLES`, `stddev > MIN_STDDEV`, parser 단계 `|corr| <= MAX_ABS_CORR`.

각 job 모듈은 `build_query()`, `parse_dataset()`, `fetch()`, `run()` 4함수 구조 통일. `run()` 의 시그니처는 job 마다 다름 (quote/source 유무) — 그래서 통일 Protocol 안 쓰고 partial 로 closure 만듦.

## 코드 스타일

- `from __future__ import annotations` + modern type syntax (`dict[str, float]`, `X | None`)
- `@dataclass(frozen=True, slots=True)` (Schedule, AppConfig)
- 로깅은 `structlog` (`logger.bind(job=..., window=...).info("updated", ...)`)
- 한국어 주석/docstring
- print/sys.stderr 금지 — 모두 structlog
- `redis.Redis` 는 type hint 용으로 `import redis` (런타임 의존)

## SQL 규약

- 1m/5m 윈도우는 `FILL(PREV)` + 확장 lookback (`MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES`, default 10) 으로 sparse symbol forward-fill.
- 10m ~ 120m 는 `SAMPLE BY 1s` raw, 240m+ 는 `SAMPLE BY 5s`.
- gap 류는 `JOIN ON symbol AND timestamp` — 두 feed 가 같은 1s 버킷에 모두 존재할 때만 집계.
- `|avg_mid_gap| ≥ 0.1` outlier 는 `filter_valid_gaps()` 로 drop. Rust `market_watcher.filter_valid_gaps` 와 임계 동일.

## Redis blob shape

Rust 컨슈머 (`market_watcher`) 와 호환되는 게 핵심. 키 변경 / shape 변경 시 Rust 쪽 deserialize 도 같이 바꿔야 함. 자세한 shape 는 `README.md` 참조.

## 테스트 구성

- **Unit** (`tests/test_{common,query_builders,parsers,scheduler,questdb_url,config}.py`) — 외부 의존 0, ms 단위.
- **Integration** (`tests/test_integration.py`) — `QUESTDB_URL` / `REDIS_URL` 살아있을 때만 실행 (없으면 module-level `pytestmark` skip). 자기 prefix (`{PREFIX}:_test_<ts>`) 만 건드리고 finally 에서 정리.

새 job 추가 시 query builder snapshot 테스트 (`test_query_builders.py`) + parser 테스트 (`test_parsers.py`) 둘 다 같이 추가하는 게 컨벤션.

## 운영 주의

- **fast/slow 분리**: `all` 모드 직렬 cycle = ~64초. fast(=≤5m) = ~8초, slow(>5m) = ~40초. 1m 윈도우가 의미 있게 갱신되려면 fast 데몬을 따로 띄우는 게 맞음. `jobs/common.py` 의 `FAST_WINDOWS` / `SLOW_WINDOWS` 가 분할 기준.
- **Heartbeat**: cycle 끝마다 `{HEARTBEAT_REDIS_PREFIX}:{mode}` 키에 `{run_count, last_cycle_ms, due, ok, duration_ms}` JSON SET. 외부 watchdog 은 `last_cycle_ms` staleness 만 봄.
- **Telegram alert**: `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` 둘 다 set 일 때 활성. `_is_cycle_failure` 정의는 "due > 0 AND ok == 0" — 부분 실패는 카운트 안 하고 (개별 schedule structlog 에 이미 찍힘) 완전 실패만. `ALERT_AFTER_CONSECUTIVE_FAILURES` (default 5) 회 연속 실패 시 1회 알림, 성공 1회 발생 시 streak 리셋 + 복구 알림. spam 방지.
- **Redis TTL 없음**: SET 만. updater 죽으면 stale blob 영원. 컨슈머가 `updated_at_ms` 로 staleness 판정 필요.

## 확장 포인트

- 새 거래소 추가 = `MARKET_GAP_QUOTE_EXCHANGES` 에 이름만 (테이블명 `<exchange>_bookticker` 만 맞으면 됨).
- 새 윈도우 추가 = `jobs/common.py` 의 `WINDOW_MINUTES` / `WINDOW_PERIOD` 에 항목 추가.
- 새 job = `jobs/<name>.py` (build_query/parse/fetch/run) + `main.build_schedules()` 등록.
