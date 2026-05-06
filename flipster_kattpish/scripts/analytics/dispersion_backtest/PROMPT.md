# 인덱스 분산 기반 Mean Reversion 알파 검증 백테스트

## 0. 환경

**중앙 QuestDB**: `211.181.122.102:9000` (HTTP), pg-wire `:8812`, db `qdb`, user/pass `admin/quest`.

**작업 디렉토리**: `/home/gate1/projects/quant/hft_monorepo/flipster_kattpish/scripts/analytics/dispersion_backtest/` — 모든 산출물(스크립트/플롯/parquet/md 보고서)을 이 안에 둘 것.

**테이블 스키마** (모든 거래소 공통, microsecond UTC TIMESTAMP):
```
timestamp, symbol, bid_price, ask_price, bid_size, ask_size
```
※ bybit/bitget/lighter/variational/pyth/flipster는 last/mark/index 추가, hyperliquid는 recv_ts_ns 추가 — 본 백테스트에서는 bid/ask만 사용.

## 1. 심볼 정규화 (하드코딩 베이스라인 — 0단계에서 데이터 존재 검증 후 수정)

```python
SYMBOL_MAP = {
    'binance':     {'BTC': 'BTC_USDT', 'ETH': 'ETH_USDT', 'SOL': 'SOL_USDT', 'BNB': 'BNB_USDT', 'XRP': 'XRP_USDT', 'DOGE': 'DOGE_USDT'},
    'okx':         {'BTC': 'BTC-USDT-SWAP', 'ETH': 'ETH-USDT-SWAP', 'SOL': 'SOL-USDT-SWAP', 'BNB': 'BNB-USDT-SWAP', 'XRP': 'XRP-USDT-SWAP', 'DOGE': 'DOGE-USDT-SWAP'},
    'bitget':      {'BTC': 'BTC_USDT', 'ETH': 'ETH_USDT', 'SOL': 'SOL_USDT', 'BNB': 'BNB_USDT', 'XRP': 'XRP_USDT', 'DOGE': 'DOGE_USDT'},
    'kucoin':      {'BTC': 'XBTUSDTM', 'ETH': 'ETHUSDTM', 'SOL': 'SOLUSDTM', 'BNB': 'BNBUSDTM', 'XRP': 'XRPUSDTM', 'DOGE': 'DOGEUSDTM'},
    'mexc':        {'BTC': 'BTC_USDT', 'ETH': 'ETH_USDT', 'SOL': 'SOL_USDT', 'BNB': 'BNB_USDT', 'XRP': 'XRP_USDT', 'DOGE': 'DOGE_USDT'},
    'gate':        {'BTC': 'BTC_USDT', 'ETH': 'ETH_USDT', 'SOL': 'SOL_USDT', 'BNB': 'BNB_USDT', 'XRP': 'XRP_USDT', 'DOGE': 'DOGE_USDT'},
    'bingx':       {'BTC': 'BTC-USDT', 'ETH': 'ETH-USDT', 'SOL': 'SOL-USDT', 'BNB': 'BNB-USDT', 'XRP': 'XRP-USDT', 'DOGE': 'DOGE-USDT'},
    'kraken':      {'BTC': 'PF_XBTUSD', 'ETH': 'PF_ETHUSD', 'SOL': 'PF_SOLUSD', 'XRP': 'PF_XRPUSD', 'DOGE': 'PF_DOGEUSD'},  # BNB 없음
    'hyperliquid': {'BTC': 'BTC', 'ETH': 'ETH', 'SOL': 'SOL', 'BNB': 'BNB', 'XRP': 'XRP', 'DOGE': 'DOGE'},
    'aster':       {'BTC': 'BTCUSDT', 'ETH': 'ETHUSDT', 'SOL': 'SOLUSDT', 'BNB': 'BNBUSDT', 'XRP': 'XRPUSDT', 'DOGE': 'DOGEUSDT'},
    'pancake':     {'BTC': 'BTCUSDT', 'ETH': 'ETHUSDT', 'SOL': 'SOLUSDT'},
    'lighter':     {'BTC': 'BTC', 'ETH': 'ETH', 'SOL': 'SOL', 'BNB': 'BNB', 'XRP': 'XRP', 'DOGE': 'DOGE'},
    'pyth':        {'BTC': 'BTC/USD', 'ETH': 'ETH/USD', 'SOL': 'SOL/USD'},
    # bybit: 메이저 BTC/ETH 데이터 없음 → 본 분석 제외 확정.
}
```

## 2. 분석 대상

- **Base symbols**: BTC, ETH, SOL (가장 풍부) — 결과 좋으면 BNB/XRP/DOGE 확장
- **인덱스 구성 거래소** (CEX 메이저, leader/lagger 분석용):
  - **Tier-A**: binance, okx, kucoin
  - **Tier-B**: bitget, gate, mexc, bingx, kraken
- **DEX 별도 분석군** (인덱스에 포함 X): hyperliquid, aster, pancake, lighter
- **Pyth**: oracle, target 아님. reference만
- **분석 윈도우**:
  - **장기 (2.5개월)**: binance + bitget + gate (제한된 풀 라인업)
  - **단기 (4일, 2026-05-02 ~ 2026-05-06)**: 거래소 풀 라인업
  - 둘 다 돌리고 일관성 비교

## 3. 데이터 정렬 / mid 계산

- pg-wire (`psycopg2` or `connectorx`) 사용. 시간 슬라이스 1h 단위 청크 로드
- mid = (bid + ask) / 2
- **1초 그리드 리샘플**, ffill **MAX 3초 한도** (3초 이상 stale은 결측 처리)
- 인덱스 계산 시 결측 거래소는 그 슬롯에서 제외하고 가중치 재정규화
- **공차 검사**: ±50bp 이상 벌어진 슬롯은 outlier flag (보통 데이터 오류)

## 4. 핵심 지표

```python
mid[ex][t]
index_mean[t] = mean({mid[ex][t]})  # 동일가중
deviation[ex][t] = (mid[ex][t] - index_mean[t]) / index_mean[t] * 1e4  # bp
sigma[ex][t] = rolling_std(deviation[ex], window=30min, right_exclusive=True)
zscore[ex][t] = deviation[ex][t] / sigma[ex][t]
```

**룩어헤드 금지**: rolling은 `[t-30min, t)` (right-exclusive). pandas `.rolling(...).std().shift(1)`.

## 5. 평균회귀 가설 검증

각 (exchange, threshold ∈ {1.0, 1.5, 2.0, 2.5, 3.0}, holding_s ∈ {1, 5, 10, 30, 60, 300}):
- 신호: `|z| > threshold` 진입 슬롯 (이전 슬롯에선 미만 — 같은 신호 중복 카운트 방지)
- direction = `-sign(z[entry])`
- forward_return_bp = `direction × (mid[ex][t+holding] - mid[ex][t]) / mid[ex][t] × 1e4`
- 통계: n, mean, std, hit_rate, t-stat, median, IQR

## 6. 비용 모델 (Flipster 캡쳐 가정)

```python
flipster_taker_bp_per_side = 0.425
flipster_maker_bp_per_side = 0.15

RT_FLIPSTER_TT = 0.425 * 2     # 0.85
RT_FLIPSTER_MT = 0.15 + 0.425  # 0.575
RT_FLIPSTER_MM = 0.15 * 2      # 0.30

HEDGE_TAKER_BP_PER_SIDE = 1.0  # 추정
SLIPPAGE_BUFFER_BP = 1.0

COST_S1 = RT_FLIPSTER_TT + 2.0 + 1.0  # 3.85 (taker 양쪽)
COST_S2 = RT_FLIPSTER_MT + 1.0 + 1.0  # 2.575 (maker entry/taker exit)
COST_S3 = RT_FLIPSTER_TT + 1.0        # 1.85 (헤지 없음 directional)
```

⚠️ Flipster 자체 데이터 stale (2026-05-04 끊김) → **Flipster mid 사용 금지**. "Flipster fair price ≈ index_mean" 가정 하에 검증.

## 7. 결과 출력

`results/`:
1. `signal_stats.parquet` — (exchange, threshold, holding, base, scenario) all combos
2. `heatmap_<base>_<exchange>.png` — threshold × holding mean PnL bp
3. `n_signals.png` — 거래소별 시간대별 신호 빈도
4. `forward_return_dist_<best_combo>.png` — 분포 + 0 기준선
5. `top10.md` — net_alpha_bp top 10 (시나리오별)
6. `regime_check.md` — 시간대 × 변동성 레짐
7. `oos_check.md` — 70/30 split 검증
8. `summary.md` — 최종 결론

## 8. Sanity / Robustness

- 거래소별 일관성 (한 거래소만 알파 → 의심)
- 시간대 (0–8 / 8–16 / 16–24 UTC)
- 변동성 레짐 (30min realized vol percentile low/mid/high)
- OOS: 70/30 time split
- n_signals < 100 조합은 결과 표시만, 결론 제외

## 9. 결론 보고서 (`summary.md`)

답해야 할 6가지:
1. 분산 캡쳐 알파가 통계적으로 유의한가? (t-stat, p-value)
2. 비용 차감 후 양수 net 알파인가? 시나리오별?
3. 어느 거래소가 가장 자주 outlier? (lagger 후보)
4. outlier 회귀 평균 시간 (best holding)
5. 시간대/레짐 안정한가? OOS 깨지나?
6. Flipster 진입 가정 시 실현 가능 (mid 차이 vs spread)

## 10. 제약사항

- 룩어헤드 금지 (rolling right-exclusive)
- 신호 중복 카운트 금지
- bybit 제외 (BTC 0)
- Flipster mid 사용 금지 (stale)
- **단계별 진행. 0단계 끝나면 멈추고 보고서 → 사용자 OK 후 다음**
