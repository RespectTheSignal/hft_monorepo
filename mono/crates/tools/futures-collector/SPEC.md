# tools/futures-collector — SPEC

## 역할
기존 `gate_hft/futures_collector` 와 동일한 목적의 utility — 여러 거래소의 symbol/contract 메타데이터를 주기적으로 REST 로 수집해 supabase/파일에 반영.

거래소 WS 엔드포인트가 운영하는 **심볼 리스트 최신화** 가 목적. hot path 외부 툴.

## 데이터
- 거래소별 `GET /exchangeInfo` 류 호출
- 응답에서 symbol, tick_size, lot_size, funding_rate, listed_at 추출
- supabase `symbols` 테이블 upsert

## Phase 1 TODO

1. `main.rs` — bg runtime 하나. tokio.
2. 각 거래소 REST 클라이언트. `hft-exchange-<v>::fetch_symbols()` 로 재사용 가능 (각 vendor SPEC 에 `available_symbols` API 있음 — 이걸 재사용).
3. supabase upsert — `postgrest-rs`.
4. 주기: `cron = "*/10 * * * *"` (10분) 또는 실행 당 1회 (systemd timer).
5. 에러 핸들링: 거래소 중 하나 실패해도 다른 거래소는 계속.

## 계약
- publisher/subscriber 와 **완전 독립**. 실행 중에 파이프라인 영향 0.
- supabase 가 갱신되면 publisher 가 다음 재시작 시 새 symbol 을 자동 로드 (hft-config 가 supabase 에서 pull).

## 완료 조건
- [ ] 5개 거래소 symbol 리스트를 1회 실행으로 전부 upsert
- [ ] supabase 장애 시 에러 로그 + nonzero exit
