# Flipster API - Essential Concepts

## 1. One-Way Trading Mode

- 현재 **One-Way 모드**만 운영 중
- 심볼당 하나의 오픈 포지션(Long 또는 Short)만 허용
- `set-trade-mode` 엔드포인트로 One-Way 모드 활성화 필요
- `reduceOnly` 플래그: 포지션을 줄이거나 완전히 닫는 주문만 허용 (새 포지션 증가 방지)

> **중요:** `MULTIPLE_POSITIONS` 모드 설정 시 모든 주문이 처리 불가합니다.

## 2. Symbol-Level Leverage & Margin Mode

- 각 심볼별로 레버리지와 마진 설정을 **개별 구성** 필요
- `set-leverage-margin-mode` 엔드포인트로 설정
- **주문 전에 반드시 설정** 필요
- Isolated Margin: 해당 포지션에만 마진 한정, 청산도 해당 포지션만 영향
- Cross Margin: 모든 오픈 포지션 간 리소스 공유

## 3. Supported Market Types & Symbols

- **Spot** 및 **Perpetual** 시장 지원
- 주문 동작과 포지션 규칙이 시장 유형별로 다름
- `get-contract-info` 및 `get-tradable-symbols`로 심볼 가용성 사전 확인 필수

## 4. Order Types

| 유형 | 설명 |
|------|------|
| **Market** | 즉시 실행, 내부적으로 IOC로 처리. `timeInForce` 파라미터 사용 금지 |
| **Limit** | 지정 가격 실행. `timeInForce`: GTC(기본값) 또는 IOC |
| **Stop-Market** | 트리거 기반 실행. `price` 파라미터가 트리거 가격 역할 |

## 5. Authentication & Expiry

- Private 요청: `api-key`, `api-expires`, `api-signature` (HMAC-SHA256) 필수
- `api-expires` 타임스탬프가 지나면 요청 무효화 → 리플레이 공격 방지
- 상세 내용: `01_authentication.md` 참조

## 6. System Stability & Rate Control

- API 키별 Rate Limit 적용
- 주문/마진 관련 요청 후 내부 지연 적용 (의도적 설계)
- **중복 요청 대신 WebSocket 스트림 활용 권장**

## 7. Key Takeaways

1. 심볼당 하나의 포지션 (One-Way Mode)
2. 거래 전 레버리지/마진 사전 설정 필수
3. `reduceOnly` 활용한 안전한 포지션 청산
4. HMAC 서명 필수
5. Rate Limit 준수
6. 실시간 데이터는 WebSocket 스트림 사용
7. 주문 전 심볼 가용성 확인
