# Flipster Pairs Paper Bot — Project Context

This is a multi-variant paper-trading bot for a **Gate↔Flipster pairs mean-reversion strategy**, running on gate1 and reading from a central shared QuestDB. The code was migrated here on 2026-04-15 from `clink@100.125.108.105:~/projects/flipster`.

**First action for a new session**: if you need deep context (empirical findings, failed experiments, parameter rationale), read the memory files at:
```
~/.claude/projects/-home-gate1-projects-quant-hft_monorepo-flipster_kattpish/memory/
```
- `project_pairs_results.md` — verified findings + 8-variant grid + failed experiments
- `project_flipster_stack_updates.md` — services, env, commands, gotchas
- `project_flipster_stack.md` — original 2026-04-13 baseline (mostly obsolete)
- `reference_flipster_ratelimit.md` — Cloudflare rate limit notes

Everything in this file is a summary of those. Read them for nuance.

---

## What's running now

**Project dir**: `/home/gate1/projects/quant/hft_monorepo/flipster_kattpish`

**Services (on gate1, our process)**:
- `target/release/collector` with `PAPER_BOT=1 PAPER_VARIANTS=1 READ_FROM_QUESTDB=1` — 8 paper bot variants in one process, NO WebSockets to exchanges
- Grafana 12.4.2 on `:3002` (admin/admin) with dashboards provisioned

**Services (central, NOT ours)**:
- **QuestDB** at `211.181.122.102` (HTTP `:9000`, pg-wire `:8812`, ILP `:9009`, admin/quest, db qdb)
- **data_publisher** (feed pipeline, not in this repo) writes `flipster_bookticker` + other exchange tables to the central QuestDB

**Data flow**:
```
central data_publisher → central QuestDB bookticker tables
                              ↓ (200ms poll)
      gate1 collector's questdb_reader → tokio broadcast<BookTick>
                              ↓
                       8 paper variants → position_log (ILP to central QuestDB)
```

Our collector **does not subscribe to exchange WebSockets**. `READ_FROM_QUESTDB=1` mode polls the shared QuestDB for new rows and re-emits them into the same broadcast channel the paper bot already subscribed to. `DISABLE_TICK_WRITES=1` prevents our collector from writing duplicate bookticker rows (feed owns those).

**Dashboard**: http://100.115.233.110:3002/d/flipster-variants

---

## The 8-variant grid (defined in `collector/src/bin/main.rs`)

All use the EWMA signal path with `PAIRS_HEDGE_VENUE=gate`.

| ID | entry σ | min_std | stop σ | Sizing | Role |
|---|---|---|---|---|---|
| A_baseline | 2.5 | 0.3 | 100 | Kelly | loose control |
| **B_minstd2** | 2.5 | 2.0 | 100 | Kelly | verified edge (~+5 bp, 80% win) |
| **C_minstd3** | 2.5 | 3.0 | 100 | Kelly | tighter edge (~+8 bp, 93% win) |
| D_high_sig | 4.0 | 0.3 | 100 | Kelly | large-move control |
| **E_stop6** | 2.5 | 2.0 | 6σ | Kelly | min_std + stop on |
| **F_minstd2_fix200** | 2.5 | 2.0 | 100 | **fixed $200** | = B with fair sizing |
| **G_minstd3_fix200** | 2.5 | 3.0 | 100 | **fixed $200** | = C with fair sizing |
| **H_minstd2_fix1000** | 2.5 | 2.0 | 100 | **fixed $1000** | size scaling test |

Kelly is **quarter-Kelly** (`KELLY_FRACTION=0.25`, cap 5x leverage). During bootstrap (<20 trades per symbol per variant) every trade is $10. Fixed-size F/G/H strip out this bootstrap noise. B/F/H emit *identical* bp results (same signal), only $ PnL differs.

---

## Hard-won empirical findings (do NOT unlearn)

1. **Binance hedge leg is dead.** 8.85 bp round-trip > any observed mispricing P99. Gate hedge (1.87 bp round-trip) is the only working config.

2. **`min_std ≥ 2.0` is the whole ballgame.** Without it, variants trade all-symbol noise and cluster at exactly `−fee` on converged exits. With it, 79-93% win rates.

3. **`PAIRS_MIN_HOLD_MS=5000` is required.** Without, microstructure noise crosses the exit band within 1-8s forcing fee-only exits. With 5s floor, hold times normalize to ~12s.

4. **Stop loss is a net-negative at σ thresholds.** `4σ` or `6σ` fire on normal volatility wiggles. `PAIRS_STOP_SIGMA=100` (disabled) beats them empirically. E_stop6 is only positive because `min_std=2.0` filters out the regime where stop is worst.

5. **`exit_sigma` direction is counter-intuitive.** Larger value → exits trigger *sooner* (more reversion uncaptured). 0.3 is correct; 0.7 is strictly worse.

6. **`MAX_MID_AGE_MS` must be ≥ several seconds.** 500ms silently drops all exits on Gate mid-cap symbols. 10000ms is the working value.

7. **Fixed-size is better than Kelly during validation.** Kelly amplifies variance during small-sample windows, makes variant comparison noisy. Kelly is optimal *if* you know the edge — we don't yet.

---

## Failed experiments (don't repeat)

### slow_avg market_watcher variants (F_slow_conf_*, H_slow_nocf_*)

Tried replacing drifting EWMA with a QuestDB-backed 60min `avg_gap_bp` baseline + 4-timescale (1s/5s/10s/20s) snapshot confirmation filter modeled on `gate_hft_rust`'s decide_order_v8.

**Result**: 11% win rate, −10.78 bp avg. Dramatically worse.

**Failure modes**:
1. **Confirmation filter was backwards** for our symmetric spread trade. gate_hft's "lagging check" fits a directional bet (laggard catches up to leader). For pair mean-reversion, requiring `|current_dev| < |prev_dev|` means entering *after* most reversion is done.
2. **slow_avg signal weaker than EWMA** even without the filter. Possible unexplored causes: 60min window too long, no volatility floor, cursor initialization.

**Current state**: `market_watcher.rs` still runs (30s refresh, publishes to `Arc<RwLock<GapState>>`) but **NO variant consumes it**. Code paths intact (`Params.gap_state`, `pairs_entry_bp`, `pairs_exit_bp`, `pairs_snapshot_confirm_min`, `snapshot_history`). Revisit would need: disable confirmation, add volatility floor, re-test at matched trade counts.

---

## Key code files

```
collector/src/strategies.rs         # pairs/latency/funding; Kelly; min_hold; fixed-size override
collector/src/market_watcher.rs     # 60min gap baseline — running but UNUSED
collector/src/questdb_reader.rs     # polls central QuestDB for BookTicks (READ_FROM_QUESTDB mode)
collector/src/ilp.rs                # write_tick honors DISABLE_TICK_WRITES
collector/src/bin/main.rs           # 8-variant spawn + READ_FROM_QUESTDB branch
collector/src/exchanges/gate.rs     # WS chunk 50 sym/frame (not used in RFQD mode)
scripts/control.sh                  # PROJECT path hardcoded to flipster_kattpish
scripts/start_grafana.sh            # default port 3002, PROJECT env override required
grafana/dashboards/05-variants.json # A~H comparison + recent trades panel
.env                                # all tunables including RFQD flags
```

---

## Env vars that matter

```bash
# Central shared QuestDB
QDB_HOST=211.181.122.102
QDB_ILP_PORT=9009
QDB_PG_PORT=8812
QUESTDB_HTTP_URL=http://211.181.122.102:9000

# Migration / deployment flags
READ_FROM_QUESTDB=1       # poll central QuestDB instead of WS
DISABLE_TICK_WRITES=1     # don't write bookticker ILP (feed owns it)

# Pairs strategy
PAIRS_HEDGE_VENUE=gate
PARAM_FEE_BP=1.87
PAIRS_ENTRY_SIGMA=2.5
PAIRS_EXIT_SIGMA=0.3
PAIRS_MIN_HOLD_MS=5000
PAIRS_MAX_HOLD_MS=60000
PAIRS_STOP_SIGMA=100.0    # disabled
PAIRS_MIN_STD_BP=0.3      # per-variant override in main.rs
PAIRS_EWMA_TAU_SEC=60
MAX_ENTRY_SPREAD_BP=5.0
MAX_MID_AGE_MS=10000

# Multi-variant
PAPER_VARIANTS=1

# Kelly sizing
BANKROLL_USD=10000
KELLY_FRACTION=0.25
KELLY_MAX_LEVERAGE=5.0
KELLY_MIN_SAMPLES=20
KELLY_BOOTSTRAP_FRAC=0.001
```

---

## Commands

```bash
cd /home/gate1/projects/quant/hft_monorepo/flipster_kattpish

# Restart paper bot
./scripts/control.sh paper-start

# Tail trade events
tail -f logs/collector.log | grep "paper close"

# Build
cargo build --release -p collector

# Start Grafana (PROJECT env REQUIRED on gate1)
PROJECT=/home/gate1/projects/quant/hft_monorepo/flipster_kattpish \
  GF_SERVER_HTTP_PORT=3002 \
  nohup ./scripts/start_grafana.sh > logs/grafana.log 2>&1 &

# Per-variant totals from central QuestDB
python3 -c "
import urllib.request, urllib.parse, json
sql = \"SELECT account_id, count() n, round(avg(pnl_bp),2) avg_bp, round(sum(pnl_bp*size/10000),2) sum_usd, round(sum(case when pnl_bp>0 then 1.0 else 0.0 end)/count()*100,1) win FROM position_log WHERE strategy='pairs' GROUP BY account_id ORDER BY account_id\"
print(json.loads(urllib.request.urlopen('http://211.181.122.102:9000/exec?query='+urllib.parse.quote(sql)).read()).get('dataset'))
"
```

---

## Symbol format gotcha (IMPORTANT)

The **central QuestDB stores Gate symbols as `BTC_USDT`** (with underscore), but our in-memory code expects `BTCUSDT` (no underscore). Two places normalize:

1. `questdb_reader::poll_once` — Gate rows get `raw_symbol.replace('_', '')` before BookTick construction
2. `market_watcher.rs` SQL — gate CTE uses `replace(symbol, '_', '') normalized` in the join

**If you add any new path that reads Gate symbols from the shared QuestDB, apply the same normalization.** Flipster symbols are `BTCUSDT.PERP` in both local and central — `base_of()` handles via `.trim_end_matches(".PERP").trim_end_matches("USDT")`.

---

## Pending decisions / next steps

1. **Longer sample (3-6 hours continuous)** — Kelly needs time to activate on 5+ symbols per variant. Results are suggestive but not robust yet.

2. **Warm-start persistence** — `symbol_stats` / `bankroll_usd` reset on every restart. Disk persistence required before live.

3. **Feed team request**: add `bid_size`/`ask_size` fields to `flipster_bookticker` (other exchanges have them). Currently filled with 0 in `questdb_reader`. Needed for depth-aware sizing, slippage model, orderbook imbalance signals — blocker for live execution.

4. **Live execution layer** — maker orders, kill switch, per-symbol drawdown caps. Not scoped yet.

5. **Real tail-risk test** — current data is all benign. Before live, add hard per-trade loss cap and daily loss kill.

6. **Sizing path** — fixed-size F/G/H for validation. Once 1000+ trades/variant with consistent positive mean, consider dynamic `max_position_size` cap (gate_hft pattern). Full-Kelly per trade NOT recommended given estimate uncertainty.

---

## Known gotchas

- **Central QuestDB is multi-tenant**. Other strategies (11+ `monitoring_questdb.py` processes) share the same db. NEVER use `TRUNCATE TABLE position_log` — it wipes everyone's data. Filter by `account_id IN (...)` or time window.
- **`PROJECT` env var required for `start_grafana.sh`** on gate1 — default `$HOME/projects/flipster` doesn't exist here.
- **Grafana dashboards use `dateadd('h', -N, now())`** instead of `$__timeFilter()` — the macro is unreliable on QuestDB's pg-wire for multi-series queries.
- **`funding_poller` not running on gate1**. Funding strategy won't fire until `funding_rate` is populated. Start manually if needed.
- **Old collector on `clink@100.125.108.105` is STOPPED** as of 2026-04-15 ~17:00 KST. Don't restart unless you want dual writes to shared QuestDB.
- **QuestDB window functions multiplied by a literal** must be wrapped in a subquery (`SELECT a*0.02 FROM (SELECT sum(x) OVER () AS a ...)`). Bare `SUM() OVER () * 0.02` errors with "Invalid column".
- **Gate WS limit ~200 subs per connection** — only relevant if you revert out of `READ_FROM_QUESTDB` mode.

---

## Migration history

- **2026-04-13**: Initial standalone setup on clink — local QuestDB + Grafana + collector + own data.
- **2026-04-14**: Multi-variant paper bot with Kelly; slow_avg experiment failed; variant grid consolidated to 8 EWMA-based (A~H).
- **2026-04-15 AM**: Migrated to gate1 at `flipster_kattpish/`. Initially kept own WS subscribers writing to central QuestDB.
- **2026-04-15 ~17:00**: User pushed back on WS duplication ("why subscribe to WS when feed already collects?"). Implemented `questdb_reader`, switched to `READ_FROM_QUESTDB=1`, dropped WS subscribers. Fixed Gate symbol normalization in two places.
