# strategy/

전략 모듈. `Strategy` Protocol 을 구현한 클래스들.

- `base.py` — `Strategy` Protocol (on_book_ticker / on_timer / on_start / on_stop)
- `sample.py` — 구조 확인용 샘플 전략 (주문 안 냄, 로깅만)
- **`basis_meanrev.py`** — Binance vs Flipster basis mean-reversion (실매매 대상)

---

## BasisMeanRev 전략

### 컨셉

두 거래소 동일 심볼의 중간가(mid) 차이 = **basis** 가 단기적으로 평균 근처로 복귀한다는 가정.

```
basis_t = fl_mid_t − bn_mid_t
z_t     = (basis_t − mean(basis, window)) / std(basis, window)
```

`z` 가 임계치를 넘으면 **Flipster 쪽** 에 반대 방향 포지션을 태우고, 평균 근처로 돌아오면 청산. Binance 는 **신호 소스**일 뿐, 매매는 Flipster(또는 gate) 단방향.

### 시그널 → Intent 전이

3-state target model: `LONG` / `SHORT` / `FLAT`

```
z ≤ −open_k   → target LONG   (Flipster 저평가 → 매수)
z ≥ +open_k   → target SHORT  (Flipster 고평가 → 매도)
|z| ≤ close_k → target FLAT   (평균 근처 → 청산)
나머지        → 현 intent 유지 (히스테리시스)
```

Intent 가 바뀌면 `max_position_size` 에 맞춘 target qty 를 고정. 현재 실제 포지션과의 delta 를 LIMIT IOC 로 보낸다. **손절/타임아웃 없음** — 신호가 반대로 바뀌어야만 포지션이 변한다.

### 필터 (4단계)

진입/flip 시에만 적용되는 4개 필터를 **AND** 통과해야 함:

1. **통계적 유의성**: `std_bps ≥ min_open_std_bps` — std 가 너무 작으면 z 계산이 noise 증폭
2. **경제적 이탈**: `dev_bps ≥ min_open_dev_bps` — |basis − mean| 이 price 대비 최소 bp 이상이어야 함 (수수료 감당)
3. **스프레드 인식 edge**: `dev_bps × β_fl ≥ (fl_spread_bps + 2 × fee_bps) × safety` — 현재 Flipster 호가 스프레드 + 왕복 수수료 대비 기대 edge 가 충분해야 함
4. **Binance cooldown**: 최근 `binance_open_cooldown_ms` ms 내에 Binance 가격이 바뀌었으면 skip (stealth)

Close 는 별도 필터 세트 (`min_close_*`, `binance_close_cooldown_ms`). 청산은 리스크 축소이므로 open 보다 느슨함. 단 `min_close_dev_bps > 0` 이면 "noise close" 방지 — basis 가 너무 작게 이탈했을 때 FLAT 으로 가지 않고 현 포지션 유지.

### 주문 모델

- 단일 LIMIT IOC, **반대편 호가 크로싱**
- `delta > 0` → BUY @ ask, `delta < 0` → SELL @ bid
- Open (`actual * delta ≥ 0`) 과 Close (`actual * delta < 0`) 가 각각 `open_order_size` / `close_order_size` 로 상한
- `|delta| > order_size` 면 여러 tick 에 걸쳐 점진 체결 (쿨다운으로 간격 조절)
- Flip (LONG → SHORT 등) 은 Close chunk → Open chunk 순서로 나뉨

Miss 시 재전송: `cooldown_ms` 경과 + 아직 target 미달이면 자동 재시도. Fill 이 확인된 tick 에서는 쿨다운 무시하고 즉시 다음 chunk.

### 파라미터 (BasisMeanRevParams)

| 그룹 | 이름 | 기본 | 설명 |
|---|---|---|---|
| 대상 | `canonicals` | — | 거래 canonical 심볼 튜플 (예: `("EPIC","ETH")`) |
| 대상 | `execution_exchange` | `flipster` | 매매 대상 거래소 (`flipster` / `gate`) |
| 시간 | `window_sec` | 30.0 | z-score 계산 윈도우 |
| 임계값 | `open_k` | 2.0 | \|z\| 이 이 값 이상이면 LONG/SHORT |
| 임계값 | `close_k` | 0.5 | \|z\| 이 이 값 이하면 FLAT |
| 사이즈 | `max_position_size` | 20.0 | 심볼당 순포지션 한도 notional |
| 사이즈 | `open_order_size` | 20.0 | 진입/확장 1주문 상한 |
| 사이즈 | `close_order_size` | 20.0 | 청산/축소 1주문 상한 |
| 사이즈 | `portfolio_max_size` | 0.0 | 전체 포트폴리오 합 한도 (0=무제한) |
| 필터 open | `min_open_dev_bps` | 3.0 | \|dev\|/price 하한 (bp) |
| 필터 open | `min_open_std_bps` | 0.5 | std/price 하한 (bp) |
| 필터 close | `min_close_dev_bps` | 1.0 | noise close 방지 하한 |
| 필터 close | `min_close_std_bps` | 0.0 | — |
| 스프레드 | `spread_aware_filter` | True | cost-aware edge check on/off |
| 스프레드 | `beta_fl_assumption` | 0.5 | 복귀 중 Flipster 기여 가정 (0~1) |
| 스프레드 | `fee_bps_cost` | 0.45 | per-side 수수료 (bp) |
| 스프레드 | `spread_edge_safety` | 1.0 | 손익분기 × safety 배수 |
| Stealth | `binance_open_cooldown_ms` | 200 | Binance 가격 변화 후 open 유예 (lead-lag 은닉) |
| Stealth | `binance_close_cooldown_ms` | 0 | 동일, close 용 |
| 기타 | `warmup_samples` | 200 | 최소 샘플 수 |
| 기타 | `cooldown_ms` | 500 | miss 후 재전송 쿨다운 |

### 환경변수 (scripts/backtest.py, main.py 공통)

env 로 런타임 override:

```
STRATEGY=basis_meanrev  DRY_RUN=1|0
CANONICALS=APR,ENJ,BEAT,...
EXEC_EXCHANGE=flipster|gate
WINDOW_SEC=30  OPEN_K=2.0  CLOSE_K=0.5
MAX_POSITION=20  OPEN_ORDER_SIZE=20  CLOSE_ORDER_SIZE=20  PORTFOLIO_MAX=100
MIN_OPEN_DEV_BPS=3.0  MIN_OPEN_STD_BPS=0.5
MIN_CLOSE_DEV_BPS=1.0  MIN_CLOSE_STD_BPS=0
SPREAD_FILTER=1  BETA_FL=0.5  FEE_BPS=0.45  SPREAD_EDGE_SAFETY=1.0
BN_OPEN_COOLDOWN_MS=200  BN_CLOSE_COOLDOWN_MS=0
COOLDOWN_MS=500
```

### 내부 상태

- `_state[fl_sym] -> _SymbolState` — intent, 고정 target_qty, last_emit_ns, last_actual
- `_bn_last_quote / _bn_last_change_ns` — Binance cooldown 판정용
- `_stats` — 전환/주문/skip 카운트

### Live vs Backtest

**전략 코드는 한 벌.** 차이는 드라이버:

| | Live | Backtest |
|---|---|---|
| 시계 | `LiveClock` (`time.time_ns`) | `SimClock` (이벤트 ts 추적) |
| 시장 데이터 | ZMQ feed | QuestDB 리플레이 |
| 주문 실행 | `FlipsterExecutionClient` (실 REST) | `FillSimulator` (100ms 지연) |
| UserState | WS private + REST | FillSimulator 가 직접 갱신 |

백테스트에서 `ParamX = Y` 로 나온 설정은 라이브에서 `env ParamX=Y` 로 그대로 돌아감.

### 실행 예

**백테스트** (scripts/backtest.py):
```bash
BN_OPEN_COOLDOWN_MS=200 MAX_POSITION=20 \
uv run python scripts/backtest.py \
  APR,ENJ,BEAT,GRIFFAIN,EPIC \
  2026-04-14T00:00:00Z 2026-04-15T00:00:00Z
```

**라이브 dry-run** (main.py):
```bash
STRATEGY=basis_meanrev DRY_RUN=1 \
CANONICALS=BEAT,ENJ,EPIC,TOWNS,GRIFFAIN \
MAX_POSITION=10 OPEN_ORDER_SIZE=10 CLOSE_ORDER_SIZE=10 \
PORTFOLIO_MAX=50 \
BN_OPEN_COOLDOWN_MS=200 MIN_CLOSE_DEV_BPS=1.0 \
uv run python -m strategy_flipster.main config.toml
```

**실매매**: 위에서 `DRY_RUN=0` 으로. ⚠ 계정 상태(포지션/대기주문) 사전 확인 필수.

### 주의사항 (실매매)

1. **Flipster API 심볼 제한** — 계정 등급에 따라 소형 알트는 `ApiSymbolRestricted` 로 거부될 수 있음. 배포 전 각 심볼로 tiny LIMIT IOC 테스트 권장.
2. **Binance lead detection** — Flipster 가 lead-lag 패턴을 감지해 API 차단할 위험. `binance_open_cooldown_ms ≥ 200` 로 운영 권장.
3. **재전송 loop** — 주문 거절 시 `cooldown_ms` 간격으로 계속 재시도. 실패율 카운트 후 상한 도달 시 전략 정지 로직은 현재 없음 → 운영 시 외부 감시 필요.
4. **Close 보호 부재** — 손절/타임아웃/강제 청산 없음. 신호 반전만으로 포지션 전환. 극단적 가격 이동 시 노출 방어 없음.
5. **MTM vs realized** — backtest 엔진은 둘 다 추적하지만, 라이브는 WS 이벤트 기반 realized 만 즉시 반영. 백테스트의 MTM DD 값이 실전의 리스크 상한에 가까움.

### 백테스트 벤치마크 (2026-04-14, 24h, 11 심볼, fee 0.45bp)

| 설정 | NET | win | MTM DD | trades |
|---|---|---|---|---|
| max=$20, cd=0 | +$529 | 85.9% | $0.65 | 48,677 |
| max=$20, cd=200 (stealth) | +$95 | 71.4% | $0.65 | 18,808 |
| max=$100, cd=0 | +$1,209 | 67.0% | $3.22 | 158,040 |
| max=$100, cd=200 | +$40 | 51.3% | **$19.52** ⚠ | 66,251 |

Cooldown 200ms stealth 모드에서는 `max=$20` 권장. `max=$100` + cooldown 조합은 big position 을 cooldown 동안 close 못 해서 MDD 폭증.

심볼별 robustness (cd=200 vs cd=0):
- **BEAT** 가장 robust (−62% 수익 감소만)
- **APR** 가장 취약 (−96%, 짧은 lead-lag 의존)
- **ETH, BREV** 는 cd=200 에서 손실 전환 → 제외 권장
