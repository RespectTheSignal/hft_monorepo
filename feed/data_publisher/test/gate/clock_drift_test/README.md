# Gate.io Clock Drift Monitor

로컬-서버 시계 오프셋을 시간에 따라 모니터링하여 clock drift를 정량화하는 테스트. HFT 시스템의 시계 동기화 주기 결정에 활용.

## Architecture

```
  Time ──────────────────────────────────────────────────▶

  t=0s        t=10s       t=20s       t=30s       ...
  ┌───┐       ┌───┐       ┌───┐       ┌───┐
  │ S │       │ S │       │ S │       │ S │       S = sync sample
  └─┬─┘       └─┬─┘       └─┬─┘       └─┬─┘           (N sub-rounds)
    │           │           │           │
    offset₀     offset₁     offset₂     offset₃
                │           │           │
                delta₁      delta₂      delta₃    delta = drift between points
```

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python clock_drift_test.py
```

## Usage

```bash
# 기본 실행 (120초, 10초 간격)
python clock_drift_test.py

# 장시간 모니터링
python clock_drift_test.py --duration 300 --interval 5

# 고정밀 측정 (sub-rounds 증가)
python clock_drift_test.py --sub-rounds 10
```

| Flag | Default | Description |
|------|---------|-------------|
| `--duration` | `120` | 총 모니터링 시간 (초) |
| `--interval` | `10` | 측정 간격 (초) |
| `--sub-rounds` | `5` | 측정 포인트당 서브 샘플 수 (median 사용) |

## Metrics

| Metric | Description |
|--------|-------------|
| **Offset** | 로컬-서버 시계 차이 (양수 = 서버가 빠름) |
| **Delta** | 연속 측정 간 오프셋 변화량 |
| **Drift rate** | 초당 오프셋 변화 속도 (μs/s) |
| **Assessment** | drift rate 기반 시계 동기화 주기 권장 |

## Drift Rate Interpretation

| Drift Rate | 평가 | 권장 re-sync 주기 |
|-----------|------|-------------------|
| < 1 μs/s | EXCELLENT | 수 분 |
| 1-10 μs/s | GOOD | 30-60초 |
| 10-100 μs/s | MODERATE | 10-15초 |
| > 100 μs/s | POOR | 연속 |

## API Reference

- REST: `https://api.gateio.ws/api/v4`
- Headers: `X-In-Time`, `X-Out-Time` (μs precision)
- [Gate.io Futures REST API docs](../../../docs/gate/05_futures_rest_api.md)
