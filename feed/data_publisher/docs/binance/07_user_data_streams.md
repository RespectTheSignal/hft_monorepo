# Binance USDⓈ-M Futures - User Data Streams

User Data Stream은 계정 변경 사항을 실시간으로 수신하기 위한 WebSocket 스트림.

## Listen Key 관리

### 1. Start User Data Stream

**`POST /fapi/v1/listenKey`**

| Property | Value |
|----------|-------|
| Weight | 1 |
| Security | USER_STREAM |

파라미터: 없음

새 User Data Stream 시작. 스트림은 **60분** 후 keepalive가 없으면 종료됨. 이미 활성 listenKey가 있으면 해당 키를 반환하고 유효 기간을 60분 연장.

**Response:**
```json
{
  "listenKey": "pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1"
}
```

---

### 2. Keepalive User Data Stream

**`PUT /fapi/v1/listenKey`**

| Property | Value |
|----------|-------|
| Weight | 1 |
| Security | USER_STREAM |

파라미터: 없음

타임아웃 방지를 위한 keepalive. **약 60분마다** ping 전송 권장.

**Response:**
```json
{
  "listenKey": "3HBntNTepshgEdjIwSUIBgB9keLyOCg5qv3n6bYAtktG8ejcaW5HXz9Vx1JgIieg"
}
```

---

### 3. Close User Data Stream

**`DELETE /fapi/v1/listenKey`**

| Property | Value |
|----------|-------|
| Weight | 1 |
| Security | USER_STREAM |

파라미터: 없음

**Response:**
```json
{}
```

---

## Event: Margin Call

**Event Type:** `MARGIN_CALL`

사용자의 포지션 위험 비율이 너무 높을 때 활성화. 이것은 위험 안내 정보일 뿐, 투자 전략 결정용이 아님. 변동성이 큰 시장에서는 이벤트 전송과 동시에 청산이 발생할 수 있음.

### Payload

```json
{
  "e": "MARGIN_CALL",
  "E": 1587727187525,
  "cw": "3.16812045",
  "p": [
    {
      "s": "ETHUSDT",
      "ps": "LONG",
      "pa": "1.327",
      "mt": "CROSSED",
      "iw": "0",
      "mp": "187.17127",
      "up": "-1.166074",
      "mm": "1.614445"
    }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `MARGIN_CALL` |
| `E` | LONG | Event timestamp (ms) |
| `cw` | STRING | Cross wallet balance (crossed position margin call 시에만) |
| `p` | ARRAY | Margin call 트리거 포지션 배열 |
| `p[].s` | STRING | Symbol |
| `p[].ps` | STRING | Position side (`LONG` / `SHORT`) |
| `p[].pa` | STRING | Position amount |
| `p[].mt` | STRING | Margin type (`CROSSED` / `ISOLATED`) |
| `p[].iw` | STRING | Isolated wallet balance (isolated position만) |
| `p[].mp` | STRING | Mark price |
| `p[].up` | STRING | Unrealized PnL |
| `p[].mm` | STRING | Maintenance margin required |

---

## Event: Balance and Position Update

**Event Type:** `ACCOUNT_UPDATE`

잔고 또는 포지션 정보 변경 시 트리거 (잔고, 포지션, 마진 타입 설정 변경 포함).

### 주요 동작
- 미체결 또는 취소 주문은 이 이벤트를 트리거하지 않음
- 변경된 포지션의 심볼만 포함
- Funding fee 변경 시:
  - Crossed position: 영향받는 자산의 잔고 업데이트만
  - Isolated position: 잔고 업데이트 + 특정 isolated 포지션 포함

### Event Reason Types (필드 `m`)

| Reason | Description |
|--------|-------------|
| `DEPOSIT` | 입금 |
| `WITHDRAW` | 출금 |
| `ORDER` | 주문 |
| `FUNDING_FEE` | 펀딩 수수료 |
| `WITHDRAW_REJECT` | 출금 거부 |
| `ADJUSTMENT` | 조정 |
| `INSURANCE_CLEAR` | 보험 기금 정산 |
| `ADMIN_DEPOSIT` | 관리자 입금 |
| `ADMIN_WITHDRAW` | 관리자 출금 |
| `MARGIN_TRANSFER` | 마진 이전 |
| `MARGIN_TYPE_CHANGE` | 마진 타입 변경 |
| `ASSET_TRANSFER` | 자산 이전 |
| `OPTIONS_PREMIUM_FEE` | 옵션 프리미엄 수수료 |
| `OPTIONS_SETTLE_PROFIT` | 옵션 정산 수익 |
| `AUTO_EXCHANGE` | 자동 교환 |
| `COIN_SWAP_DEPOSIT` | 코인 스왑 입금 |
| `COIN_SWAP_WITHDRAW` | 코인 스왑 출금 |

### Payload

```json
{
  "e": "ACCOUNT_UPDATE",
  "E": 1564745798939,
  "T": 1564745798938,
  "a": {
    "m": "ORDER",
    "B": [
      {
        "a": "USDT",
        "wb": "122624.12345678",
        "cw": "100.12345678",
        "bc": "50.12345678"
      }
    ],
    "P": [
      {
        "s": "BTCUSDT",
        "pa": "0",
        "ep": "0.00000",
        "bep": "0",
        "cr": "200",
        "up": "0",
        "mt": "isolated",
        "iw": "0.00000000",
        "ps": "BOTH"
      },
      {
        "s": "BTCUSDT",
        "pa": "20",
        "ep": "6563.66500",
        "bep": "0",
        "cr": "0",
        "up": "2850.21200",
        "mt": "isolated",
        "iw": "13200.70726908",
        "ps": "LONG"
      }
    ]
  }
}
```

### Root Level

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `ACCOUNT_UPDATE` |
| `E` | LONG | Event timestamp (ms) |
| `T` | LONG | Transaction timestamp (ms) |
| `a` | OBJECT | Update data |

### Update Data (`a`)

| Field | Type | Description |
|-------|------|-------------|
| `m` | STRING | Event reason type |
| `B` | ARRAY | Balance 업데이트 배열 |
| `P` | ARRAY | Position 업데이트 배열 |

### Balance Object (`B[]`)

| Field | Type | Description |
|-------|------|-------------|
| `a` | STRING | Asset name |
| `wb` | STRING | Wallet balance |
| `cw` | STRING | Cross wallet balance |
| `bc` | STRING | Balance change (PnL 및 commission 제외) |

### Position Object (`P[]`)

| Field | Type | Description |
|-------|------|-------------|
| `s` | STRING | Symbol |
| `pa` | STRING | Position amount |
| `ep` | STRING | Entry price |
| `bep` | STRING | Breakeven price |
| `cr` | STRING | 수수료 차감 전 누적 실현 손익 |
| `up` | STRING | Unrealized PnL |
| `mt` | STRING | Margin type (`isolated` / `crossed`) |
| `iw` | STRING | Isolated wallet balance (isolated만) |
| `ps` | STRING | Position side (`LONG`, `SHORT`, `BOTH`) |

---

## Event: Order Update

**Event Type:** `ORDER_TRADE_UPDATE`

새 주문 생성 또는 주문 상태 변경 시 푸시.

### Enum Values

**Side:** `BUY`, `SELL`

**Order Type:** `LIMIT`, `MARKET`, `STOP`, `STOP_MARKET`, `TAKE_PROFIT`, `TAKE_PROFIT_MARKET`, `TRAILING_STOP_MARKET`, `LIQUIDATION`

**Execution Type:** `NEW`, `CANCELED`, `CALCULATED` (Liquidation), `EXPIRED`, `TRADE`, `AMENDMENT` (Order Modified)

**Order Status:** `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `EXPIRED`, `EXPIRED_IN_MATCH`

**Time in Force:** `GTC`, `IOC`, `FOK`, `GTX`

**Working Type:** `MARK_PRICE`, `CONTRACT_PRICE`

### Expiry Reason Codes

| Code | Description |
|------|-------------|
| 0 | None (default) |
| 1 | Self-trading 방지를 위해 만료 |
| 2 | IOC 주문 미체결 잔량 취소 |
| 3 | IOC self-trade 방지 미체결 잔량 취소 |
| 4 | 우선순위 높은 RO 주문 또는 포지션 반전에 의해 취소 |
| 5 | 계정 청산 중 만료 |
| 6 | GTE 조건 미충족 |
| 7 | 심볼 상장 폐지 |
| 8 | Stop 트리거 후 초기 주문 만료 |
| 9 | Market 주문 미체결 잔량 취소 |

### Payload

```json
{
  "e": "ORDER_TRADE_UPDATE",
  "E": 1568879465651,
  "T": 1568879465650,
  "o": {
    "s": "BTCUSDT",
    "c": "TEST",
    "S": "SELL",
    "o": "TRAILING_STOP_MARKET",
    "f": "GTC",
    "q": "0.001",
    "p": "0",
    "ap": "0",
    "sp": "7103.04",
    "x": "NEW",
    "X": "NEW",
    "i": 8886774,
    "l": "0",
    "z": "0",
    "L": "0",
    "N": "USDT",
    "n": "0",
    "T": 1568879465650,
    "t": 0,
    "b": "0",
    "a": "9.91",
    "m": false,
    "R": false,
    "wt": "CONTRACT_PRICE",
    "ot": "TRAILING_STOP_MARKET",
    "ps": "LONG",
    "cp": false,
    "AP": "7476.89",
    "cr": "5.0",
    "pP": false,
    "si": 0,
    "ss": 0,
    "rp": "0",
    "V": "EXPIRE_TAKER",
    "pm": "OPPONENT",
    "gtd": 0,
    "er": "0"
  }
}
```

### Root Level

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `ORDER_TRADE_UPDATE` |
| `E` | LONG | Event time (ms) |
| `T` | LONG | Transaction time (ms) |
| `o` | OBJECT | Order object |

### Order Object (`o`)

| Field | Type | Description |
|-------|------|-------------|
| `s` | STRING | Symbol |
| `c` | STRING | Client order ID. 특수: `autoclose-XXX` (청산), `adl_autoclose` (ADL), `settlement_autoclose-` (정산) |
| `S` | STRING | Side: `BUY` / `SELL` |
| `o` | STRING | Order type |
| `f` | STRING | Time in force |
| `q` | STRING | Original quantity |
| `p` | STRING | Original price |
| `ap` | STRING | Average price |
| `sp` | STRING | Stop price (TRAILING_STOP_MARKET에서는 무시) |
| `x` | STRING | Execution type |
| `X` | STRING | Order status |
| `i` | LONG | Order ID |
| `l` | STRING | Order last filled quantity |
| `z` | STRING | Order filled accumulated quantity |
| `L` | STRING | Last filled price |
| `N` | STRING | Commission asset |
| `n` | STRING | Commission amount |
| `T` | LONG | Order trade time |
| `t` | LONG | Trade ID |
| `b` | STRING | Bids notional |
| `a` | STRING | Ask notional |
| `m` | BOOLEAN | Is maker side trade |
| `R` | BOOLEAN | Reduce-only flag |
| `wt` | STRING | Stop price working type |
| `ot` | STRING | Original order type |
| `ps` | STRING | Position side |
| `cp` | BOOLEAN | Close-all flag (conditional order) |
| `AP` | STRING | Activation price (TRAILING_STOP_MARKET only) |
| `cr` | STRING | Callback rate (TRAILING_STOP_MARKET only) |
| `pP` | BOOLEAN | Price protection status |
| `si` | LONG | (Ignore) |
| `ss` | LONG | (Ignore) |
| `rp` | STRING | Realized profit of trade |
| `V` | STRING | STP mode |
| `pm` | STRING | Price match mode |
| `gtd` | LONG | GTD order auto-cancel time |
| `er` | STRING | Expiry reason code |

---

## Event: Account Configuration Update

**Event Type:** `ACCOUNT_CONFIG_UPDATE`

계정 설정 변경 시 트리거. 변경 유형에 따라 payload가 다름.

### Format 1: Leverage Change

```json
{
  "e": "ACCOUNT_CONFIG_UPDATE",
  "E": 1611646737479,
  "T": 1611646737476,
  "ac": {
    "s": "BTCUSDT",
    "l": 25
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `ACCOUNT_CONFIG_UPDATE` |
| `E` | LONG | Event timestamp (ms) |
| `T` | LONG | Transaction time (ms) |
| `ac.s` | STRING | Trading pair symbol |
| `ac.l` | INT | Leverage value |

### Format 2: Multi-Assets Margin Mode Change

```json
{
  "e": "ACCOUNT_CONFIG_UPDATE",
  "E": 1611646737479,
  "T": 1611646737476,
  "ai": {
    "j": true
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `e` | STRING | Event type: `ACCOUNT_CONFIG_UPDATE` |
| `E` | LONG | Event timestamp (ms) |
| `T` | LONG | Transaction time (ms) |
| `ai.j` | BOOLEAN | Multi-Assets margin mode status |
