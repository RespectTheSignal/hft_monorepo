# Flipster API Documentation - Overview

## Host

- **Production URL:** `https://trading-api.flipster.io`
- **Base Path:** `/api/v1/`
- **Protocol:** HTTPS only (데이터 무결성 및 보안을 위해 필수)
- **Sandbox/Staging:** 향후 제공 예정

## Supported Trading Modes and Symbols

| 항목 | 지원 현황 |
|------|-----------|
| Trading Mode | One-Way (Multi-position 예정) |
| Margin Mode | Isolated, Cross 모두 지원 |
| Order Types | Market, Limit, Stop-Market (Time Trigger 예정) |
| Market Type | Spot, Perpetual (Perp) |

> **주의:** 플랫폼에 표시되는 모든 심볼이 API 거래에 항상 사용 가능한 것은 아닙니다. 주문 전 반드시 심볼 가용성을 확인해야 합니다.

## API Naming Standard (카테고리)

| Category | Purpose | 인증 필요 |
|----------|---------|-----------|
| `trade` | 주문 생성/수정/취소, 포지션 관리, 레버리지 조정, 마진 조작 | Yes |
| `market` | 실시간 시장 데이터: 티커, 오더북, 캔들스틱 | Yes (read) |
| `account` | 사용자 계정 요약, 잔고, 포지션 | Yes (read) |
| `public` | 서버 시간, 시스템 상태 | No |
| `affiliate` | 추천 네트워크 관리, 커미션 추적, CSV 내보내기 (등록된 Affiliate 전용) | Yes |

## Rate Limiting & System Stability

### Rate Limiting
- 각 API 키별로 **RPS (requests per second)** 및 **RPM (requests per minute)** 할당량 부여
- 초과 시 `429 Too Many Requests` 에러와 함께 임시 제한
- 엔드포인트 유형(public, market, trade, account)에 따라 제한 다름
- 응답의 `Retry-After` 헤더 시간 후 안전하게 재시도 가능

### Adaptive Delay Mechanism
- 주문 및 마진 관련 요청 후 짧은 밀리초 지연이 자동 적용
- 시스템의 순차 처리 보장, 동시성 충돌 및 주문 큐잉 이슈 최소화를 위한 의도적 설계
- 성능 문제가 아닌 Flipster 내부 로드밸런싱 전략의 일부
