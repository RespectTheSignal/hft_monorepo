# Gate.io BookTicker Jitter Analysis

`book_ticker` 메시지의 도착 간격 변동, 레이턴시 변동, 버스트 패턴을 분석하는 테스트. HFT 시스템의 타이밍 예측 가능성 평가에 활용.

## Architecture

```
  Messages arriving over time:

  ──|──|──|──────────|──||──|──|──────|──|──|──▶ time
    ↑  ↑  ↑          ↑  ↑↑  ↑  ↑      ↑  ↑  ↑
    regular          gap  burst  regular  gap

  Analysis targets:
  1. Inter-arrival time distribution
  2. End-to-end latency variance (jitter)
  3. Burst detection (< 1ms apart)
  4. Gap detection (> 5x median interval)
```

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python jitter_analysis_test.py
```

## Usage

```bash
# 기본 실행 (SOL_USDT, 200 samples)
python jitter_analysis_test.py

# 더 많은 샘플 수집
python jitter_analysis_test.py --contract BTC_USDT --samples 500
```

| Flag | Default | Description |
|------|---------|-------------|
| `--contract` | `SOL_USDT` | 측정 대상 futures contract |
| `--samples` | `200` | 수집할 메시지 수 |
| `--sync-rounds` | `10` | Clock sync 라운드 수 |

## Metrics

### Inter-Arrival Time
| Metric | Description |
|--------|-------------|
| **Inter-arrival** | 연속 메시지 간 로컬 수신 시간 차이 |
| **Message rate** | 초당 메시지 수신 빈도 (msg/s) |

### Latency Jitter
| Metric | Description |
|--------|-------------|
| **Latency** | 전체 end-to-end 레이턴시 분포 |
| **Jitter** | 연속 메시지 간 레이턴시 변동 \|Δ\| |

### Pattern Detection
| Metric | Description |
|--------|-------------|
| **Burst** | 1ms 미만 간격으로 도착한 메시지 클러스터 |
| **Gap** | median 간격의 5배 이상인 대기 구간 |

### Stability Metrics
| Metric | Description |
|--------|-------------|
| **Latency CoV** | 레이턴시 변동계수 (stdev/mean). < 0.2 = stable |
| **Inter-arrival CoV** | 도착 간격 변동계수. < 0.5 = regular |
| **p99/p50 ratio** | tail amplification — 높을수록 간헐적 지연 심함 |

## Output Includes

- Percentile 통계 (min, p50, p90, p95, p99, max, avg, std)
- ASCII 히스토그램 (Inter-arrival time, End-to-end latency 분포)
- Burst/Gap 탐지 결과
- Stability 종합 평가

## API Reference

- WebSocket: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- REST: `https://api.gateio.ws/api/v4`
- Channel: `futures.book_ticker`
- [Gate.io Futures WebSocket docs](../../../docs/gate/06_futures_websocket.md)
