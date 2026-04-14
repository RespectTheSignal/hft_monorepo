# Binance USDⓈ-M Futures API - Error Codes

## General Errors (-1xxx)

| Code | Name | Description |
|------|------|-------------|
| -1000 | UNKNOWN | An unknown error occurred while processing the request |
| -1001 | DISCONNECTED | Internal error; unable to process your request. Please try again |
| -1002 | UNAUTHORIZED | You are not authorized to execute this request |
| -1003 | TOO_MANY_REQUESTS | Rate limit exceeded; IP may be temporarily banned |
| -1004 | DUPLICATE_IP | This IP is already on the white list |
| -1005 | NO_SUCH_IP | No such IP has been white listed |
| -1006 | UNEXPECTED_RESP | Unexpected response from message bus |
| -1007 | TIMEOUT | Timeout waiting for response from backend server |
| -1008 | REQUEST_THROTTLED | Server overloaded or request throttled by system protection |
| -1010 | ERROR_MSG_RECEIVED | Error message received |
| -1011 | NON_WHITE_LIST | This IP cannot access this route |
| -1013 | INVALID_MESSAGE | Invalid message format |
| -1014 | UNKNOWN_ORDER_COMPOSITION | Unsupported order combination |
| -1015 | TOO_MANY_ORDERS | Too many new orders |
| -1016 | SERVICE_SHUTTING_DOWN | This service is no longer available |
| -1020 | UNSUPPORTED_OPERATION | This operation is not supported |
| -1021 | INVALID_TIMESTAMP | Timestamp for this request is outside of the recvWindow |
| -1022 | INVALID_SIGNATURE | Signature for this request is not valid |
| -1023 | START_TIME_GREATER_THAN_END_TIME | Start time is greater than end time |
| -1099 | NOT_FOUND | Not found, unauthenticated, or unauthorized |

## Parameter Errors (-11xx)

| Code | Name | Description |
|------|------|-------------|
| -1100 | ILLEGAL_CHARS | Illegal characters found in a parameter |
| -1101 | TOO_MANY_PARAMETERS | Too many parameters sent for this endpoint |
| -1102 | MANDATORY_PARAM_EMPTY_OR_MALFORMED | A mandatory parameter was not sent, was empty/null, or malformed |
| -1103 | UNKNOWN_PARAM | An unknown parameter was sent |
| -1104 | UNREAD_PARAMETERS | Not all sent parameters were read |
| -1105 | PARAM_EMPTY | A parameter was empty |
| -1106 | PARAM_NOT_REQUIRED | A parameter was sent when not required |
| -1108 | BAD_ASSET | Invalid asset |
| -1109 | BAD_ACCOUNT | Invalid account |
| -1110 | BAD_INSTRUMENT_TYPE | Invalid symbolType |
| -1111 | BAD_PRECISION | Precision is over the maximum defined for this asset |
| -1112 | NO_DEPTH | No orders on book for symbol |
| -1113 | WITHDRAW_NOT_NEGATIVE | Withdrawal amount must be negative |
| -1114 | TIF_NOT_REQUIRED | TimeInForce parameter sent when not required |
| -1115 | INVALID_TIF | Invalid timeInForce |
| -1116 | INVALID_ORDER_TYPE | Invalid orderType |
| -1117 | INVALID_SIDE | Invalid side |
| -1118 | EMPTY_NEW_CL_ORD_ID | New client order ID was empty |
| -1119 | EMPTY_ORG_CL_ORD_ID | Original client order ID was empty |
| -1120 | BAD_INTERVAL | Invalid interval |
| -1121 | BAD_SYMBOL | Invalid symbol |
| -1122 | INVALID_SYMBOL_STATUS | Invalid symbol status |
| -1125 | INVALID_LISTEN_KEY | This listenKey does not exist |
| -1126 | ASSET_NOT_SUPPORTED | This asset is not supported |
| -1127 | MORE_THAN_XX_HOURS | Lookup interval is too big |
| -1128 | OPTIONAL_PARAMS_BAD_COMBO | Combination of optional parameters invalid |
| -1130 | INVALID_PARAMETER | Invalid data sent for a parameter |
| -1136 | INVALID_NEW_ORDER_RESP_TYPE | Invalid newOrderRespType |

## Order Errors (-2xxx)

| Code | Name | Description |
|------|------|-------------|
| -2010 | NEW_ORDER_REJECTED | Order submission rejected |
| -2011 | CANCEL_REJECTED | Cancel request failure as open order not found in orderbook |
| -2012 | CANCEL_ALL_FAIL | Batch cancel failure |
| -2013 | NO_SUCH_ORDER | Order does not exist |
| -2014 | BAD_API_KEY_FMT | API-key format invalid |
| -2015 | REJECTED_MBX_KEY | Invalid API-key, IP, or permissions for action |
| -2016 | NO_TRADING_WINDOW | No trading window could be found for the symbol |
| -2017 | API_KEYS_LOCKED | API Keys are locked on this account |
| -2018 | BALANCE_NOT_SUFFICIENT | Balance is insufficient |
| -2019 | MARGIN_NOT_SUFFICIENT | Margin is insufficient |
| -2020 | UNABLE_TO_FILL | Unable to fill |
| -2021 | ORDER_WOULD_IMMEDIATELY_TRIGGER | Order would immediately trigger |
| -2022 | REDUCE_ONLY_REJECT | ReduceOnly Order is rejected |
| -2023 | USER_IN_LIQUIDATION | User in liquidation mode now |
| -2024 | POSITION_NOT_SUFFICIENT | Position is not sufficient |
| -2025 | MAX_OPEN_ORDER_EXCEEDED | Reach max open order limit |
| -2026 | REDUCE_ONLY_ORDER_TYPE_NOT_SUPPORTED | This OrderType is not supported when reduceOnly |
| -2027 | MAX_LEVERAGE_RATIO | Exceeded the maximum allowable position at current leverage |
| -2028 | MIN_LEVERAGE_RATIO | Leverage is smaller than permitted |

## Filter/Validation Errors (-4xxx)

| Code | Name | Description |
|------|------|-------------|
| -4000 | INVALID_ORDER_STATUS | Invalid order status |
| -4001 | PRICE_LESS_THAN_ZERO | Price less than 0 |
| -4002 | PRICE_GREATER_THAN_MAX_PRICE | Price greater than max price |
| -4003 | QTY_LESS_THAN_ZERO | Quantity less than zero |
| -4004 | QTY_LESS_THAN_MIN_QTY | Quantity less than min quantity |
| -4005 | QTY_GREATER_THAN_MAX_QTY | Quantity greater than max quantity |
| -4006 | STOP_PRICE_LESS_THAN_ZERO | Stop price less than zero |
| -4007 | STOP_PRICE_GREATER_THAN_MAX_PRICE | Stop price greater than max price |
| -4008 | TICK_SIZE_LESS_THAN_ZERO | Tick size less than zero |
| -4009 | MAX_PRICE_LESS_THAN_MIN_PRICE | Max price less than min price |
| -4010 | MAX_QTY_LESS_THAN_MIN_QTY | Max qty less than min qty |
| -4011 | STEP_SIZE_LESS_THAN_ZERO | Step size less than zero |
| -4012 | MAX_NUM_ORDERS_LESS_THAN_ZERO | Max num orders less than zero |
| -4013 | PRICE_LESS_THAN_MIN_PRICE | Price less than min price |
| -4014 | PRICE_NOT_INCREASED_BY_TICK_SIZE | Price not increased by tick size |
| -4015 | INVALID_CL_ORD_ID_LEN | Client order id length should not be more than 36 chars |
| -4016 | PRICE_HIGHER_THAN_MULTIPLIER_UP | Price is higher than mark price multiplier cap |
| -4017 | MULTIPLIER_UP_LESS_THAN_ZERO | Multiplier up less than zero |
| -4018 | MULTIPLIER_DOWN_LESS_THAN_ZERO | Multiplier down less than zero |
| -4019 | COMPOSITE_SCALE_OVERFLOW | Composite scale too large |
| -4020 | TARGET_STRATEGY_INVALID | Invalid target strategy for order type |
| -4021 | INVALID_DEPTH_LIMIT | Invalid depth limit |
| -4022 | WRONG_MARKET_STATUS | Market status sent is not valid |
| -4023 | QTY_NOT_INCREASED_BY_STEP_SIZE | Qty not increased by step size |
| -4024 | PRICE_LOWER_THAN_MULTIPLIER_DOWN | Price is lower than mark price multiplier floor |
| -4025 | MULTIPLIER_DECIMAL_LESS_THAN_ZERO | Multiplier decimal less than zero |
| -4026 | COMMISSION_INVALID | Commission invalid |
| -4027 | INVALID_ACCOUNT_TYPE | Invalid account type |
| -4028 | INVALID_LEVERAGE | Invalid leverage |
| -4029 | INVALID_TICK_SIZE_PRECISION | Tick size precision is invalid |
| -4030 | INVALID_STEP_SIZE_PRECISION | Step size precision is invalid |
| -4031 | INVALID_WORKING_TYPE | Invalid parameter working type |
| -4032 | EXCEED_MAX_CANCEL_ORDER_SIZE | Exceed maximum cancel order size |
| -4033 | INSURANCE_ACCOUNT_NOT_FOUND | Insurance account not found |
| -4044 | INVALID_BALANCE_TYPE | Balance Type is invalid |
| -4045 | MAX_STOP_ORDER_EXCEEDED | Reach max stop order limit |
| -4046 | NO_NEED_TO_CHANGE_MARGIN_TYPE | No need to change margin type |
| -4047 | THERE_EXISTS_OPEN_ORDERS | Margin type cannot be changed if there exists open orders |
| -4048 | THERE_EXISTS_QUANTITY | Margin type cannot be changed if there exists position |
| -4049 | ADD_ISOLATED_MARGIN_REJECT | Add margin only support for isolated position |
| -4050 | CROSS_BALANCE_INSUFFICIENT | Cross balance insufficient |
| -4051 | ISOLATED_BALANCE_INSUFFICIENT | Isolated balance insufficient |
| -4052 | NO_NEED_TO_CHANGE_AUTO_ADD_MARGIN | No need to change auto add margin |
| -4053 | AUTO_ADD_CROSSED_MARGIN_REJECT | Auto add margin only support for isolated position |
| -4054 | ADD_ISOLATED_MARGIN_NO_POSITION_REJECT | Cannot add position margin: position is 0 |
| -4055 | AMOUNT_MUST_BE_POSITIVE | Amount must be positive |
| -4056 | INVALID_API_KEY_TYPE | Invalid api key type |
| -4057 | INVALID_RSA_PUBLIC_KEY | Invalid api public key |
| -4058 | MAX_PRICE_TOO_LARGE | maxPrice and priceDecimal too large |
| -4059 | NO_NEED_TO_CHANGE_POSITION_SIDE | No need to change position side |
| -4060 | INVALID_POSITION_SIDE | Invalid position side |
| -4061 | POSITION_SIDE_NOT_MATCH | Order's position side does not match user's setting |
| -4062 | REDUCE_ONLY_CONFLICT | Invalid or improper reduceOnly value |
| -4067 | POSITION_SIDE_CHANGE_EXISTS_OPEN_ORDERS | Position side cannot be changed if there exists open orders |
| -4068 | POSITION_SIDE_CHANGE_EXISTS_QUANTITY | Position side cannot be changed if there exists position |
| -4082 | INVALID_BATCH_PLACE_ORDER_SIZE | Invalid number of batch place orders |
| -4083 | PLACE_BATCH_ORDERS_FAIL | Fail to place batch orders |
| -4084 | UPCOMING_METHOD | Method is not allowed currently. Upcoming soon |
| -4085 | INVALID_NOTIONAL_LIMIT_COEF | Invalid notional limit coefficient |
| -4086 | INVALID_PRICE_SPREAD_THRESHOLD | Invalid price spread threshold |
| -4087 | REDUCE_ONLY_ORDER_PERMISSION | User can only place reduce only order |
| -4088 | NO_PLACE_ORDER_PERMISSION | User can not place order currently |
| -4104 | INVALID_CONTRACT_TYPE | Invalid contract type |
| -4109 | INACTIVE_ACCOUNT | Inactive account |
| -4114 | INVALID_CLIENT_TRAN_ID_LEN | Client tran id length should be less than 64 chars |
| -4115 | DUPLICATED_CLIENT_TRAN_ID | Client tran id should be unique within 7 days |
| -4116 | DUPLICATED_CLIENT_ORDER_ID | clientOrderId is duplicated |
| -4117 | STOP_ORDER_TRIGGERING | Stop order is triggering |
| -4118 | REDUCE_ONLY_MARGIN_CHECK_FAILED | Reduce-only order failed margin requirements |
| -4120 | STOP_ORDER_SWITCH_ALGO | Order type not supported. Use Algo Order API |
| -4131 | MARKET_ORDER_REJECT | Counterparty's best price doesn't meet PERCENT_PRICE filter |
| -4135 | INVALID_ACTIVATION_PRICE | Invalid activation price |
| -4137 | QUANTITY_EXISTS_WITH_CLOSE_POSITION | Quantity must be zero with closePosition equals true |
| -4138 | REDUCE_ONLY_MUST_BE_TRUE | Reduce only must be true with closePosition equals true |
| -4139 | ORDER_TYPE_CANNOT_BE_MKT | Order type can not be market if it's unable to cancel |
| -4140 | INVALID_OPENING_POSITION_STATUS | Invalid symbol status for opening position |
| -4141 | SYMBOL_ALREADY_CLOSED | Symbol is closed |
| -4142 | STRATEGY_INVALID_TRIGGER_PRICE | Take profit or stop order will be triggered immediately |
| -4144 | INVALID_PAIR | Invalid pair |
| -4161 | ISOLATED_LEVERAGE_REJECT_WITH_POSITION | Leverage reduction not supported with open positions |
| -4164 | MIN_NOTIONAL | Order's notional must be no smaller than 5.0 |
| -4165 | INVALID_TIME_INTERVAL | Invalid time interval |
| -4167 | ISOLATED_REJECT_WITH_JOINT_MARGIN | Cannot adjust to Multi-Assets mode with isolated |
| -4168 | JOINT_MARGIN_REJECT_WITH_ISOLATED | Cannot adjust to isolated mode under Multi-Assets mode |
| -4169 | JOINT_MARGIN_REJECT_WITH_MB | Cannot adjust Multi-Assets Mode with insufficient margin balance |
| -4170 | JOINT_MARGIN_REJECT_WITH_OPEN_ORDER | Cannot adjust Multi-Assets Mode with open orders |
| -4171 | NO_NEED_TO_CHANGE_JOINT_MARGIN | Adjusted asset mode already set |
| -4172 | JOINT_MARGIN_REJECT_WITH_NEGATIVE_BALANCE | Cannot adjust with negative wallet balance |
| -4183 | PRICE_HIGHER_THAN_LIMIT | Limit price can't be higher than threshold |
| -4184 | PRICE_LOWER_THAN_STOP_MULTIPLIER_DOWN | Limit price can't be lower than threshold |
| -4192 | COOLING_OFF_PERIOD | Trade forbidden due to Cooling-off Period |
| -4202 | ADJUST_LEVERAGE_KYC_FAILED | Intermediate Personal Verification required for leverage over 20x |
| -4203 | ADJUST_LEVERAGE_ONE_MONTH_FAILED | More than 20x leverage available one month after registration |
| -4205 | ADJUST_LEVERAGE_X_DAYS_FAILED | High leverage available after account registration period |
| -4206 | ADJUST_LEVERAGE_KYC_LIMIT | Users in your location can only access max leverage of N |
| -4208 | ADJUST_LEVERAGE_ACCOUNT_SYMBOL_FAILED | Current symbol leverage cannot exceed 20 with position limit adjustment |
| -4209 | ADJUST_LEVERAGE_SYMBOL_FAILED | The max leverage of Symbol is 20x |
| -4210 | STOP_PRICE_HIGHER_THAN_LIMIT | Stop price can't be higher than threshold |
| -4211 | STOP_PRICE_LOWER_THAN_LIMIT | Stop price can't be lower than threshold |
| -4400 | TRADING_QUANTITATIVE_RULE | Futures Trading Quantitative Rules violated, only reduceOnly allowed |
| -4401 | LARGE_POSITION_SYM_RULE | Futures Trading Risk Control Rules violated, only reduceOnly allowed |
| -4402 | COMPLIANCE_BLACK_SYMBOL_RESTRICTION | Feature not available in your region per Terms of Use |
| -4403 | ADJUST_LEVERAGE_COMPLIANCE_FAILED | Leverage restricted per local regulations |

## Execution Errors (-5xxx)

| Code | Name | Description |
|------|------|-------------|
| -5021 | FOK_ORDER_REJECT | FOK order rejected as could not be filled immediately |
| -5022 | GTX_ORDER_REJECT | Post Only order rejected as could not be executed as maker |
| -5024 | MOVE_ORDER_NOT_ALLOWED_SYMBOL_REASON | Symbol not in trading status. Order amendment not permitted |
| -5025 | LIMIT_ORDER_ONLY | Only limit order is supported |
| -5026 | EXCEED_MAXIMUM_MODIFY_ORDER_LIMIT | Exceed maximum modify order limit |
| -5027 | SAME_ORDER | No need to modify the order |
| -5028 | ME_RECVWINDOW_REJECT | Timestamp for this request is outside of the ME recvWindow |
| -5029 | MODIFICATION_MIN_NOTIONAL | Order's notional must be no smaller than threshold |
| -5037 | INVALID_PRICE_MATCH | Invalid price match |
| -5038 | UNSUPPORTED_ORDER_TYPE_PRICE_MATCH | Price match only supports LIMIT, STOP AND TAKE_PROFIT |
| -5039 | INVALID_SELF_TRADE_PREVENTION_MODE | Invalid self trade prevention mode |
| -5040 | FUTURE_GOOD_TILL_DATE | goodTillDate must be greater than current time plus 600 seconds |
| -5041 | BBO_ORDER_REJECT | No depth matches this BBO order |
| -5043 | EXISTING_PENDING_MODIFICATION | A pending modification already exists for this order |
