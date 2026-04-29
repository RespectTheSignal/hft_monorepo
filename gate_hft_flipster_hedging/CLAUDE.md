# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

<!-- team-reporter: enabled -->

## What this directory is

`gate_hft_flipster_hedging/` is currently **empty** — a workspace slot inside the `hft_monorepo` for an upcoming project that hedges Gate HFT positions on Flipster (or vice versa). When you start work here, assume you're building a new service that consumes existing feeds and drives the existing order clients; you are not starting in a vacuum.

Git root is one level up at `/home/gate1/projects/quant/hft_monorepo`.

## Neighbors you will almost certainly use

Each has its own README / CLAUDE.md — read them, don't duplicate here.

| Path (relative to git root) | What it gives you |
|---|---|
| `feed/data_publisher`, `data_subscriber`, `questdb_uploader` | Flipster WS → ZMQ PUB (:7000, 96B C-struct) → IPC (`/tmp/flipster_data_subscriber.sock`) → QuestDB |
| `flipster_web/` (Py + Rust) | Reverse-engineered Flipster REST order client. Browser-cookie auth via Chrome-over-noVNC + CDP. `__cf_bm` cookie expires ~30 min |
| `gate_web/` (Py) | Same shape for Gate.io futures |
| `strategy_flipster_python/` | Strategy runner consuming exchange ZMQ + Flipster IPC; `main.py` is the event loop reference |
| `flipster_kattpish/` (symlink → `/home/gate1/projects/quant/flipster_kattpish`) | Rust workspace: `collector` + `flipster_rust`. Pairs paper bot, 8-variant grid, QuestDB-backed strategies. **Read its CLAUDE.md before touching anything hedging-related** — it contains hard-won empirical findings (Binance hedge is dead, `min_std ≥ 2.0` is load-bearing, etc.) |
| `fx_feed/` | FX bookticker probe across Binance / dxFeed / Databento |

## Shared infrastructure (NOT owned by this repo)

- **Central QuestDB** at `211.181.122.102` — HTTP `:9000`, pg-wire `:8812`, ILP `:9009`, admin/quest, db `qdb`. **Multi-tenant** — other strategies share it. Never `TRUNCATE`; filter by `account_id` or time window.
- **Central `data_publisher`** feeds the shared `flipster_bookticker` and other exchange bookticker tables. On gate1, prefer reading from central QuestDB over opening your own exchange WS (see `flipster_kattpish` `READ_FROM_QUESTDB=1`).

## Wire formats (authoritative refs, don't reinvent)

- Generic exchange bookticker (120 B): `strategy_flipster_python/src/strategy_flipster/types.py` + `feed/data_publisher/README.md`. Framing `[1B type_len][type][payload]`, type = `bookticker`.
- Flipster bookticker: 96 B ZMQ multipart `[topic][payload]`; 104 B IPC `[4B topic_len LE][topic][4B payload_len LE][payload]`.
- **Gate symbol normalization**: central QuestDB stores `BTC_USDT`; in-memory code expects `BTCUSDT`. Any new reader from shared QuestDB must strip the underscore. Existing sites in `flipster_kattpish`: `questdb_reader::poll_once`, `market_watcher.rs`.

## Build & run

No top-level runner, no repo-wide CI. Per-subproject:

```bash
# Rust crates under feed/* and the flipster_kattpish workspace
cargo build --release                 # inside the crate or workspace dir

# Python projects (fx_feed, strategy_flipster_python)
uv sync
PYTHONPATH=src uv run python -m <module> config.toml

# Multi-service control for the kattpish stack (QuestDB, collector, funding, Grafana)
cd ../flipster_kattpish && ./scripts/control.sh {start|stop|status|restart|paper-start}

# Grafana on gate1 — PROJECT env is REQUIRED (default path doesn't exist)
PROJECT=/home/gate1/projects/quant/hft_monorepo/flipster_kattpish \
  GF_SERVER_HTTP_PORT=3002 \
  ./scripts/start_grafana.sh
```

## Cross-cutting conventions

- **Code comments and docs are Korean.** Match the surrounding file.
- **Python**: `@dataclass(frozen=True, slots=True)` + `Protocol`. No inheritance, no global state, type hints required. asyncio + uvloop. Structure so each module could be re-implemented in Rust (`dataclass → struct`, `Protocol → trait`).
- **Rust**: tokio; `tracing` in newer crates, `log4rs` / `log` in `flipster_rust`. ZMQ via `zmq` crate, QuestDB via `questdb-rs` ILP.
- **Web order clients** (`flipster_web`, `gate_web`) run an Xvfb + Chrome + x11vnc + noVNC stack. Ports don't collide: Flipster noVNC `:6080` / CDP `:9222`, Gate noVNC `:6081` / CDP `:9223`. Flipster `__cf_bm` must be refreshed every ~30 min.

## Where to dig for context before designing the hedging layer

- `flipster_kattpish/CLAUDE.md` — pairs strategy grid, Kelly sizing, empirical findings. Deeper memory at `~/.claude/projects/-home-gate1-projects-quant-hft_monorepo-flipster_kattpish/memory/`.
- `strategy_flipster_python/CLAUDE.md` + `README.md` — event loop, feed protocols, `OrderRequest` / `UserState` shape.
- `flipster_web/README.md`, `gate_web/README.md` — full REST API specs, latency benchmarks (Flipster open min ~64 ms Rust / ~87 ms Py; Gate similar).
- `feed/data_publisher/README.md` — canonical 96 B wire format.

## Team reporter

The Stop hook forwards each response's transcript to a local judge LLM that decides whether to push to Telegram / Obsidian. Don't call the report endpoint by hand. If the user explicitly asks to share, end the response with a 1–2 sentence factual summary of what shipped — the judge weights explicit requests higher.
