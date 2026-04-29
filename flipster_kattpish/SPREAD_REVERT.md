# spread_revert — setup & ops guide for Claude

Mean-reversion strategy on the Binance↔Flipster basis. Reads pre-computed
30-min `bf_avg_gap_bp` and 10-min per-venue `avg_spread_bp` from the
in-memory `SharedBaselines` map populated by `baseline_writer`. Writes
trades to QuestDB `position_log` tagged `strategy='spread_revert'`.

## Code map

| File | Role |
|---|---|
| `collector/src/spread_revert.rs` | strategy logic — `run()`, `on_tick()`, `sweep_exits()` |
| `collector/src/baseline_writer.rs` | 5-sec QuestDB query loop → `SharedBaselines` |
| `collector/src/bin/main.rs` | spawns both inside the `PAPER_BOT=1` branch (search `SPREAD_REVERT`) |
| `.env.spread_revert.example` | full env template — copy & tweak |
| `scripts/control.sh` | layered env loading + paper-start/sr-start commands |

## How signals flow

```
ZMQ ticks (binance + flipster + gate)
   │
   ├──► broadcast::channel<BookTick>
   │          │
   │          ├──► spread_revert::run()        ──► position_log (paper rows)
   │          │       │                            trade_signal (audit log)
   │          │       └──► coordinator::route_signal
   │          │                  │
   │          │                  └──► ZMQ PUB (SIGNAL_PUB_ADDR)
   │          │                              │
   │          │                              ▼
   │          │                       executor (live mode only)
   │          │
   │          └──► other paper bots (gate_lead, pairs variants)
   │
   └──► QuestDB (binance_bookticker, flipster_bookticker)
              │
              └──► baseline_writer (every 5s SQL) ──► SharedBaselines map
                                                            │
                                                            ▼
                                                  spread_revert reads
```

## Entry / exit math

```
costs       = avg_flipster_spread + avg_binance_spread/2 + flipster_fee_bp
edge_short  = (current_gap - bf_avg_gap) - costs - entry_threshold_bp   → SHORT Flipster
edge_long   = (bf_avg_gap - current_gap) - costs - entry_threshold_bp   → LONG Flipster
```

Exit reasons (`position_log.exit_reason`):
- **stop** — single-leg adverse price move ≥ `SR_STOP_BP` from entry
- **timeout** — held longer than `SR_MAX_HOLD_S` (default 24 h)
- **reverse** — opposite-side entry signal triggered (implicit TP)

## Run — paper

```bash
cd ~/projects/quant/hft_monorepo/flipster_kattpish
cp .env.spread_revert.example .env.spread_revert
# edit values (especially SR_ACCOUNT_ID — distinct paper vs live)

ENV_FILE=$PWD/.env.spread_revert ./scripts/control.sh paper-start
```

Verify:
```bash
tail -f logs/collector.log | grep -E 'spread_revert|baseline_writer'
# expect: "[baseline_writer] refreshed shared baselines n_bases=395"
#         "[spread_revert] starting (baseline-driven) ..."
#         "[spread_revert] OPEN base=BTC side=long gap_bp=+5.2 ..."
```

## Run — live

Live differs from paper in TWO places only:
1. Set `SR_ACCOUNT_ID=SR_LIVE_v1` (or whatever) so QuestDB rows are tagged distinctly.
2. Run an `executor` instance subscribed to the collector's signal PUB.

```bash
ENV_FILE=$PWD/.env.spread_revert ./scripts/control.sh paper-start  # collector emits signals
./scripts/control.sh sr-start                                       # executor consumes them
```

`sr-start` invokes `target/release/executor --variant SR_LIVE_v1
--margin Isolated --trade-mode MULTIPLE_POSITIONS ...` (see control.sh
`start_sr()`).

## Check PnL

```bash
# Headline (last 1h)
curl -sG "http://211.181.122.102:9000/exec" --data-urlencode \
 "query=SELECT count() n, round(avg(pnl_bp),2) avg_bp,
  round(sum(case when pnl_bp>0 then 1.0 else 0.0 end)/count()*100,1) win_pct,
  round(sum(pnl_bp*size/10000),3) sum_usd
  FROM position_log WHERE strategy='spread_revert'
  AND timestamp > dateadd('h',-1,now())"

# By exit_reason
curl -sG "http://211.181.122.102:9000/exec" --data-urlencode \
 "query=SELECT exit_reason, count() n, round(avg(pnl_bp),2) avg_bp,
  round(sum(pnl_bp*size/10000),3) sum_usd
  FROM position_log WHERE strategy='spread_revert' GROUP BY exit_reason ORDER BY sum_usd DESC"

# Top symbols (by absolute PnL contribution)
curl -sG "http://211.181.122.102:9000/exec" --data-urlencode \
 "query=SELECT symbol, count() n, round(avg(pnl_bp),2) avg_bp,
  round(sum(pnl_bp*size/10000),3) sum_usd
  FROM position_log WHERE strategy='spread_revert'
  AND timestamp > dateadd('h',-6,now())
  GROUP BY symbol ORDER BY abs(sum_usd) DESC LIMIT 20"
```

Filter paper vs live with `account_id IN (...)` or pair with `mode='paper'`.

## Tunables (SR_*)

| env | default | meaning |
|---|---|---|
| `SR_ACCOUNT_ID` | `SPREAD_REVERT_v1` | position_log tag — set unique per paper/live |
| `SR_SIZE_USD` | 10 | notional per trade |
| `SR_MAX_POSITIONS` | 50 | global concurrent cap (safety ceiling) |
| `SR_MAX_POSITIONS_PER_BASE` | 5 | per-symbol stack cap |
| `SR_ENTRY_BP` | 2.0 | extra bp on top of costs to fire entry |
| `SR_STOP_BP` | 20 | adverse single-leg move that triggers stop |
| `SR_MAX_HOLD_S` | 86400 | hard timeout (24 h) |
| `SR_ENTRY_COOLDOWN_MS` | 500 | per-base re-entry block |
| `SR_FEE_BP` | 0.425 | per-side flipster taker fee |
| `SR_WHITELIST` | (empty = all) | comma-sep bases, uppercase |
| `SR_BLACKLIST` | M,BSB,SWARMS,ORCA | always applied, overrides whitelist |

## control.sh env layering

`scripts/control.sh` loads env files in this order (later overrides earlier):

1. `$PROJECT/.env`             — project defaults
2. `$PROJECT/.env.local`       — gitignored local overrides
3. `$PWD/.env`                 — current-folder overlay (if invoked outside `$PROJECT`)
4. `$ENV_FILE`                 — explicit overlay path

So `ENV_FILE=$PWD/.env.spread_revert ./scripts/control.sh paper-start`
inherits everything from `.env` (QuestDB creds, ZMQ endpoints) and just
overrides the SR_* knobs.

## Gotchas

- **Cold start delay**: `baseline_writer` needs ~5 s for its first SQL
  cycle. `spread_revert::run()` waits up to 30 s for the map to
  populate; if QuestDB is slow it logs a warning and falls through —
  entries stay idle until the map fills.
- **Bases without baseline**: per-base entries skip silently if
  `baselines.get(&base)` returns None. Stop/timeout exits still fire.
  This is normal during the first few minutes after start.
- **Signal port collision**: `SIGNAL_PUB_ADDR` defaults to `tcp://127.0.0.1:7500`.
  If running a second collector instance, override to `:7501` (the
  example file already does this).
- **paper vs live tag**: `mode='paper'` is hardcoded in
  `spread_revert::log_close()` — distinguish runs via `account_id`,
  not `mode`.
- **No history field**: the strategy does NOT maintain a local rolling
  window. All averages come from `SharedBaselines`. If you grep for
  `entry.history` and don't find it, that's correct — it was removed
  in commit `6712ce4`.
- **Open count is derived**: there's no atomic counter; the global cap
  check sums `entry.open.len()` across state under the existing mutex.
  Cheap but iterates ~400 bases.

## Code conventions to preserve

- Read `baselines` BEFORE taking `state.lock()` — keeps the per-tick
  critical section short.
- All four close paths (stop / timeout / reverse / sweep) push to
  `close_acts` and call `log_close()` outside the mutex. Don't add a
  fifth path that closes inline.
- `SR_ACCOUNT_ID` flows into both `position_log` AND
  `coordinator::route_signal()` — keep them in sync.
- Cooldown is per-base, not global. Don't move `last_entry_at` out of
  `BaseState`.
