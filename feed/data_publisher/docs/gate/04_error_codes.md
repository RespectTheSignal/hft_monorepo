# Gate.io API v4 Error Codes

## HTTP Status Codes

| Code | Description |
|------|-------------|
| 200/201 | Request succeeded |
| 202 | Accepted, processing not done yet |
| 204 | Succeeded, no body |
| 400 | Invalid request |
| 401 | Authentication failed |
| 404 | Not found |
| 429 | Too many requests |
| 5xx | Server error |

## REST API Error Response Format

```json
{
  "label": "ERROR_LABEL",
  "message": "Human readable error description"
}
```

## WebSocket Error Codes

| Code | Message |
|------|---------|
| 1 | `invalid argument struct` (futures) / `Invalid request body format` (spot) |
| 2 | `invalid argument` (futures) / `Invalid argument provided` (spot) |
| 3 | `service error` (futures) / `Server side error happened` (spot) |
| 4 | `authentication fail` |

## Complete Error Labels by Category

### Request / Format

| Label | Description |
|-------|-------------|
| `INVALID_PARAM_VALUE` | Invalid parameter value |
| `INVALID_PROTOCOL` | Invalid protocol |
| `INVALID_ARGUMENT` | Invalid argument |
| `INVALID_REQUEST_BODY` | Invalid request body |
| `MISSING_REQUIRED_PARAM` | Missing required parameter |
| `BAD_REQUEST` | Bad request |
| `INVALID_CONTENT_TYPE` | Invalid content type |
| `NOT_ACCEPTABLE` | Not acceptable |
| `METHOD_NOT_ALLOWED` | Method not allowed |
| `NOT_FOUND` | Not found |

### Authentication

| Label | Description |
|-------|-------------|
| `INVALID_CREDENTIALS` | Invalid credentials |
| `INVALID_KEY` | Invalid API key |
| `IP_FORBIDDEN` | IP not in whitelist |
| `READ_ONLY` | Key has read-only permission |
| `INVALID_SIGNATURE` | Invalid signature |
| `MISSING_REQUIRED_HEADER` | Missing required header |
| `REQUEST_EXPIRED` | Request timestamp too far from server time |
| `ACCOUNT_LOCKED` | Account is locked |
| `FORBIDDEN` | Forbidden |

### Wallet

| Label | Description |
|-------|-------------|
| `SUB_ACCOUNT_NOT_FOUND` | Sub-account not found |
| `SUB_ACCOUNT_LOCKED` | Sub-account is locked |
| `MARGIN_BALANCE_EXCEPTION` | Margin balance exception |
| `MARGIN_TRANSFER_FAILED` | Margin transfer failed |
| `TOO_MUCH_FUTURES_AVAILABLE` | Too much futures available |
| `FUTURES_BALANCE_NOT_ENOUGH` | Futures balance not enough |
| `ACCOUNT_EXCEPTION` | Account exception |
| `SUB_ACCOUNT_TRANSFER_FAILED` | Sub-account transfer failed |
| `ADDRESS_NOT_USED` | Address not used |
| `TOO_FAST` | Request too fast |
| `WITHDRAWAL_OVER_LIMIT` | Withdrawal over limit |
| `API_WITHDRAW_DISABLED` | API withdraw disabled |
| `INVALID_WITHDRAW_ID` | Invalid withdrawal ID |
| `INVALID_WITHDRAW_CANCEL_STATUS` | Invalid withdrawal cancel status |
| `DUPLICATE_REQUEST` | Duplicate request |
| `ORDER_EXISTS` | Order already exists |
| `INVALID_CLIENT_ORDER_ID` | Invalid client order ID |

### Spot and Margin

| Label | Description |
|-------|-------------|
| `INVALID_PRECISION` | Invalid precision |
| `INVALID_CURRENCY` | Invalid currency |
| `INVALID_CURRENCY_PAIR` | Invalid currency pair |
| `POC_FILL_IMMEDIATELY` | POC order would fill immediately |
| `ORDER_NOT_FOUND` | Order not found |
| `ORDER_CLOSED` | Order is closed |
| `ORDER_CANCELLED` | Order is cancelled |
| `QUANTITY_NOT_ENOUGH` | Quantity not enough |
| `BALANCE_NOT_ENOUGH` | Balance not enough |
| `MARGIN_NOT_SUPPORTED` | Margin not supported |
| `MARGIN_BALANCE_NOT_ENOUGH` | Margin balance not enough |
| `AMOUNT_TOO_LITTLE` | Amount too little |
| `AMOUNT_TOO_MUCH` | Amount too much |
| `REPEATED_CREATION` | Repeated creation |
| `LOAN_NOT_FOUND` | Loan not found |
| `LOAN_RECORD_NOT_FOUND` | Loan record not found |
| `NO_MATCHED_LOAN` | No matched loan |
| `NOT_MERGEABLE` | Not mergeable |
| `NO_CHANGE` | No change |
| `REPAY_TOO_MUCH` | Repay too much |
| `TOO_MANY_CURRENCY_PAIRS` | Too many currency pairs |
| `TOO_MANY_ORDERS` | Too many orders |
| `MIXED_ACCOUNT_TYPE` | Mixed account type |
| `AUTO_BORROW_TOO_MUCH` | Auto borrow too much |
| `TRADE_RESTRICTED` | Trade restricted |
| `FOK_NOT_FILL` | FOK order not filled |
| `INITIAL_MARGIN_TOO_LOW` | Initial margin too low |
| `NO_MERGEABLE_ORDERS` | No mergeable orders |
| `ORDER_BOOK_NOT_FOUND` | Order book not found |
| `FAILED_RETRIEVE_ASSETS` | Failed to retrieve assets |
| `CANCEL_FAIL` | Cancel failed |

### Futures

| Label | Description |
|-------|-------------|
| `USER_NOT_FOUND` | User not found |
| `CONTRACT_NO_COUNTER` | Contract has no counter |
| `CONTRACT_NOT_FOUND` | Contract not found |
| `RISK_LIMIT_EXCEEDED` | Risk limit exceeded |
| `INSUFFICIENT_AVAILABLE` | Insufficient available balance |
| `LIQUIDATE_IMMEDIATELY` | Would liquidate immediately |
| `LEVERAGE_TOO_HIGH` | Leverage too high |
| `LEVERAGE_TOO_LOW` | Leverage too low |
| `ORDER_NOT_FOUND` | Order not found |
| `ORDER_NOT_OWNED` | Order not owned by user |
| `ORDER_FINISHED` | Order already finished |
| `TOO_MANY_ORDERS` | Too many open orders |
| `POSITION_CROSS_MARGIN` | Position is in cross margin mode |
| `POSITION_IN_LIQUIDATION` | Position is being liquidated |
| `POSITION_IN_CLOSE` | Position is being closed |
| `POSITION_EMPTY` | Position is empty |
| `REMOVE_TOO_MUCH` | Removing too much margin |
| `RISK_LIMIT_NOT_MULTIPLE` | Risk limit not a valid multiple |
| `RISK_LIMIT_TOO_HIGH` | Risk limit too high |
| `RISK_LIMIT_TOO_LOW` | Risk limit too low |
| `PRICE_TOO_DEVIATED` | Price too deviated from mark price |
| `SIZE_TOO_LARGE` | Size too large |
| `SIZE_TOO_SMALL` | Size too small |
| `PRICE_OVER_LIQUIDATION` | Price over liquidation price |
| `PRICE_OVER_BANKRUPT` | Price over bankrupt price |
| `ORDER_POC_IMMEDIATE` | POC order would fill immediately |
| `INCREASE_POSITION` | Would increase position |
| `CONTRACT_IN_DELISTING` | Contract is being delisted |
| `POSITION_NOT_FOUND` | Position not found |
| `POSITION_DUAL_MODE` | Position is in dual mode |
| `ORDER_PENDING` | Order is pending |
| `POSITION_HOLDING` | Position is held |
| `REDUCE_EXCEEDED` | Reduce exceeded |
| `NO_CHANGE` | No change |
| `AMEND_WITH_STOP` | Cannot amend with stop |
| `ORDER_FOK` | FOK order issue |

### Collateral Loan

| Label | Description |
|-------|-------------|
| `COL_NOT_ENOUGH` | Collateral not enough |
| `COL_TOO_MUCH` | Collateral too much |
| `INIT_LTV_TOO_HIGH` | Initial LTV too high |
| `REDEEMED_LTV_TOO_HIGH` | Redeemed LTV too high |
| `BORROWABLE_NOT_ENOUGH` | Borrowable amount not enough |
| `ORDER_TOO_MANY_TOTAL` | Total orders too many |
| `ORDER_TOO_MANY_DAILY` | Daily orders too many |
| `ORDER_TOO_MANY_USER` | User orders too many |
| `ORDER_NOT_EXIST` | Order does not exist |
| `ORDER_FINISHED` | Order already finished |
| `ORDER_NO_PAY` | Order has no payment |
| `ORDER_EXIST` | Order already exists |
| `ORDER_HISTORY_EXIST` | Order history exists |
| `ORDER_REPAYING` | Order is being repaid |
| `ORDER_LIQUIDATING` | Order is being liquidated |
| `BORROW_TOO_LITTLE` | Borrow amount too little |
| `BORROW_TOO_LARGE` | Borrow amount too large |
| `REPAY_AMOUNT_INVALID` | Repay amount invalid |
| `REPAY_GREATER_THAN_AVAILABLE` | Repay greater than available |
| `POOL_BALANCE_NOT_ENOUGH` | Pool balance not enough |
| `CURRENCY_SETTLING` | Currency is settling |
| `RISK_REJECT` | Risk rejected |
| `LOAN_FAILED` | Loan failed |

### Portfolio

| Label | Description |
|-------|-------------|
| `USER_LIAB` | User has liabilities |
| `USER_PENDING_ORDERS` | User has pending orders |
| `MODE_SET` | Mode already set |

### Earn

| Label | Description |
|-------|-------------|
| `ERR_BALANCE_NOT_ENOUGH` | Balance not enough |
| `ERR_PRODUCT_SELL_OUT` | Product sold out |
| `ERR_PRODUCT_BUY` | Product buy error |
| `ERR_CREATE_ORDER` | Create order error |
| `ERR_QUOTA_LOWER_LIMIT` | Below quota lower limit |
| `ERR_QUOTA_SUPERIOR_LIMIT` | Above quota upper limit |
| `ERR_ORDER_NUMBER_LIMIT` | Order number limit |
| `ERR_PRODUCT_CLOSE` | Product closed |
| `COPIES_NOT_ENOUGH` | Copies not enough |
| `COPIES_TOO_SMALL` | Copies too small |
| `COPIES_TOO_BIG` | Copies too big |
| `TOTAL_AMOUNT_24` | 24h total amount limit |
| `TOTAL_BUYCOUNT_24` | 24h total buy count limit |
| `REDEEM_24_LIMIT` | 24h redeem limit |

### Flash Convert

| Label | Description |
|-------|-------------|
| `PRICE_OBSOLETE` | Price is obsolete |
| `QUOTA_NOT_ENOUGH` | Quota not enough |
| `SERVER_TIMEOUT` | Server timeout |
| `CONVERT_PREVIEW_EXPIRED` | Convert preview expired |
| `CONVERT_PREVIEW_NOT_MATCH` | Convert preview not match |

### Server

| Label | Description |
|-------|-------------|
| `INTERNAL` | Internal error |
| `SERVER_ERROR` | Server error |
| `TOO_BUSY` | Server too busy |
