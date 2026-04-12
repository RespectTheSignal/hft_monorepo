# Flipster API - Affiliate Endpoints

Affiliate 엔드포인트는 **등록된 Flipster Affiliate 전용**입니다.
비 Affiliate 사용자의 요청은 `400 Bad Request` (`UserNotKol`) 에러를 반환합니다.

---

## 1. Download Referee History

- **Method:** `GET`
- **Path:** `/api/v1/affiliate/overview-referral-history-csv`
- **Description:** Affiliate 대시보드의 Referee History 데이터를 CSV 파일로 다운로드

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `startDate` | string | Optional | 시작일 `YYYY-MM-DD` (inclusive). 생략 시 전체 기간 |
| `endDate` | string | Optional | 종료일 `YYYY-MM-DD` (inclusive). 생략 시 전체 기간 |
| `username` | string | Optional | 추천인 사용자명 필터 |
| `referralCode` | string | Optional | 하위 Affiliate의 추천 코드 (해당 사용자 데이터 조회) |

### Response (200 OK)

**Content-Type:** `text/csv`

### CSV 컬럼 (19개 필드)

| # | Column | Description |
|---|--------|-------------|
| 1 | `date` | 형식: YYYYMMDDTHH |
| 2 | `totalRewards` | 일일 총 보상 (USDT) |
| 3 | `activeReferees` | 활성 추천인 수 |
| 4 | `totalTradingVolume` | 보너스 포함 총 거래량 (USDT) |
| 5 | `totalEligibleTradingVolume` | 보너스 제외 총 거래량 (USDT) |
| 6 | `totalTradingFee` | 보너스 포함 총 거래 수수료 (USDT) |
| 7 | `totalEligibleTradingFee` | 보너스 제외 총 거래 수수료 (USDT) |
| 8 | `referrerUid` | 추천인 식별자 |
| 9 | `signUpTime` | 추천인 등록 시간 |
| 10 | `kycStatusOfReferee` | KYC 인증 상태 |
| 11 | `firstDepositTime` | 최초 입금 시간 |
| 12 | `totalAssetsUsdt` | 추천인 계정 자산 (USDT) |
| 13 | `username` | 추천인 사용자명 |
| 14 | `uid` | 추천인 고유 ID |
| 15 | `referralType` | `AFFILIATE` 또는 `REFERRAL` |
| 16 | `vipTier` | 등급 분류 |
| 17 | `takerFee` | 적용 수수료율 |
| 18 | `reward` | 개별 보상 (USDT) |
| 19 | `tradingVolume`, `eligibleTradingVolume`, `tradingFee`, `eligibleTradingFee`, `effectiveCommissionRate` | 추가 필드 |

### Error Responses

| Status | Error Code | Description |
|--------|-----------|-------------|
| 400 | `invalidDateAffiliateOverview` | 잘못된 날짜 범위 |
| 400 | `UserNotKol` | Affiliate가 아닌 사용자 |
| 400 | `InvalidParameter` | 잘못된 추천 코드 형식 |
| 401 | `Unauthorized` | 누락/잘못된 API 키 |
| 403 | `Forbidden` | 추천 코드가 호출자의 하위 Affiliate가 아님 |

---

## 2. Get User Info

- **Method:** `GET`
- **Path:** `/api/v1/affiliate/user-info`
- **Description:** 인증된 사용자의 UID, 추천 코드, 가입 시 사용한 추천 코드 반환

### Request Parameters

없음

### Response (200 OK)

```json
{
  "uid": "user_unique_id_string",
  "referralCode": "REFER123ABC",
  "signUpCode": "UPLINE456DEF"
}
```

### Fields

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `uid` | string | No | 사용자 고유 식별자 (Crockford Base32) |
| `referralCode` | string | No | 사용자 자신의 추천 코드 |
| `signUpCode` | string | Yes | 가입 시 사용한 추천 코드 (추천 없이 가입 시 null) |

### Error Responses

| Status | Error Code | Description |
|--------|-----------|-------------|
| 400 | `UserUIDCreationFailed` | 재시도 후에도 UID 생성 실패 |
| 401 | `Unauthorized` | 인증 실패 |
| 500 | `ReferralCodeFailToIssue` | 재시도 후에도 추천 코드 발급 실패 |

### 참고

- UID 및 추천 코드는 누락 시 **온디맨드로 자동 생성** (Lazy creation)
