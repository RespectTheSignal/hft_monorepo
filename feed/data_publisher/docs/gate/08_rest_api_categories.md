# Gate.io REST API v4 - Full Endpoint Categories

> Base URL: `https://api.gateio.ws/api/v4`
> Version: v4.106.58

## Withdrawal

| Method | Endpoint | Description | Auth |
|--------|----------|-------------|------|
| POST | `/withdrawals` | Withdraw | Yes |
| POST | `/withdrawals/push` | UID transfer | Yes |
| DELETE | `/withdrawals/{id}` | Cancel withdrawal | Yes |

## Wallet

| Method | Endpoint | Description | Auth |
|--------|----------|-------------|------|
| GET | `/wallet/currency_chains` | Query chains for currency | No |
| GET | `/wallet/deposit_address` | Generate deposit address | Yes |
| GET | `/wallet/deposits` | Deposit records | Yes |
| GET | `/wallet/withdrawals` | Withdrawal records | Yes |
| POST | `/wallet/transfers` | Transfer between accounts | Yes |
| POST | `/wallet/sub_account_transfers` | Transfer main <-> sub | Yes |
| GET | `/wallet/sub_account_transfers` | Transfer records main/sub | Yes |
| POST | `/wallet/sub_account_to_sub_account` | Sub to sub transfer | Yes |
| GET | `/wallet/withdraw_status` | Withdrawal status | Yes |
| GET | `/wallet/sub_account_balances` | Sub account balances | Yes |
| GET | `/wallet/sub_account_margin_balances` | Sub margin balances | Yes |
| GET | `/wallet/sub_account_futures_balances` | Sub futures balances | Yes |
| GET | `/wallet/sub_account_cross_margin_balances` | Sub cross margin | Yes |
| GET | `/wallet/total_balance` | Personal total balance | Yes |
| GET | `/wallet/small_balance` | Convertible small balances | Yes |
| POST | `/wallet/small_balance` | Convert small balance | Yes |
| GET | `/wallet/small_balance_history` | Conversion history | Yes |
| GET | `/wallet/getLowCapExchangeList` | Low-liquidity tokens | Yes |

## SubAccount

| Method | Endpoint | Description | Auth |
|--------|----------|-------------|------|
| POST | `/sub_accounts` | Create sub-account | Yes |
| GET | `/sub_accounts` | List sub-accounts | Yes |
| GET | `/sub_accounts/{user_id}` | Get sub-account details | Yes |
| POST | `/sub_accounts/{user_id}/keys` | Create sub API key | Yes |
| GET | `/sub_accounts/{user_id}/keys` | List sub API keys | Yes |
| GET | `/sub_accounts/{user_id}/keys/{key}` | Get sub API key | Yes |
| PUT | `/sub_accounts/{user_id}/keys/{key}` | Update sub API key | Yes |
| DELETE | `/sub_accounts/{user_id}/keys/{key}` | Delete sub API key | Yes |
| POST | `/sub_accounts/{user_id}/lock` | Lock/unlock sub | Yes |
| GET | `/sub_accounts/{user_id}/mode` | Get sub mode | Yes |
| GET | `/sub_accounts/unified_mode` | List sub unified modes | Yes |

## Unified Account

| Method | Endpoint | Description | Auth |
|--------|----------|-------------|------|
| GET | `/unified/accounts` | Get unified account info | Yes |
| GET | `/unified/account_balance` | Query balance | Yes |
| POST | `/unified/loans` | Borrow or repay | Yes |
| GET | `/unified/loans` | List loan records | Yes |
| GET | `/unified/interest_records` | Interest records | Yes |
| GET | `/unified/risk_units` | Risk unit details | Yes |
| PUT | `/unified/unified_mode` | Set unified mode | Yes |
| GET | `/unified/currency_discount_tiers` | Currency discount tiers | No |
| GET | `/unified/loan_margin_tiers` | Loan margin tiers | No |
| GET | `/unified/leverage/user_currency_config` | Loan currency leverage | Yes |
| POST | `/unified/leverage/user_currency_setting` | Set loan leverage | Yes |
| POST | `/unified/collateral_currencies` | Set collateral currency | Yes |

## Isolated Margin

| Method | Endpoint | Description | Auth |
|--------|----------|-------------|------|
| GET | `/margin/accounts` | Margin account list | Yes |
| GET | `/margin/account_book` | Balance change history | Yes |
| GET | `/margin/funding_accounts` | Funding account list | Yes |
| POST | `/margin/auto_repay` | Update auto repay | Yes |
| GET | `/margin/auto_repay` | Query auto repay | Yes |
| GET | `/margin/transferable` | Max transferable amount | Yes |
| GET | `/margin/uni/currency_pairs` | Lending markets | No |
| GET | `/margin/uni/currency_pairs/{pair}` | Single pair info | No |
| POST | `/margin/uni/loans` | Borrow or repay | Yes |
| GET | `/margin/uni/loans` | List loans | Yes |
| GET | `/margin/uni/loan_records` | Loan records | Yes |
| GET | `/margin/uni/borrowable` | Borrowable amount | Yes |
| GET | `/margin/uni/interest_records` | Interest records | Yes |
| GET | `/margin/leverage/user_market_config` | User leverage | Yes |
| POST | `/margin/leverage/user_market_setting` | Set leverage | Yes |
| GET | `/margin/account_list` | Isolated user accounts | Yes |

## Spot

| Method | Endpoint | Description | Auth |
|--------|----------|-------------|------|
| GET | `/spot/currencies` | All currencies | No |
| GET | `/spot/currencies/{currency}` | Single currency | No |
| GET | `/spot/currency_pairs` | All pairs | No |
| GET | `/spot/currency_pairs/{pair}` | Single pair | No |
| GET | `/spot/tickers` | Tickers | No |
| GET | `/spot/order_book` | Order book | No |
| GET | `/spot/trades` | Market trades | No |
| GET | `/spot/candlesticks` | Candlesticks | No |
| GET | `/spot/fee` | User fee rates | Yes |
| GET | `/spot/accounts` | Spot accounts | Yes |
| GET | `/spot/account_book` | Account book | Yes |
| POST | `/spot/orders` | Create order | Yes |
| GET | `/spot/open_orders` | All open orders | Yes |
| POST | `/spot/cross_liquidate_orders` | Close position | Yes |
| POST | `/spot/batch_orders` | Batch create orders | Yes |
| GET | `/spot/orders` | List orders | Yes |
| DELETE | `/spot/orders` | Cancel all open orders | Yes |
| POST | `/spot/cancel_batch_orders` | Cancel batch by IDs | Yes |
| GET | `/spot/orders/{order_id}` | Get single order | Yes |
| DELETE | `/spot/orders/{order_id}` | Cancel single order | Yes |
| PATCH | `/spot/orders/{order_id}` | Amend order | Yes |
| GET | `/spot/my_trades` | Personal trade history | Yes |
| GET | `/spot/time` | Server time | No |
| POST | `/spot/countdown_cancel_all` | Countdown cancel | Yes |
| POST | `/spot/batch_countdown_cancel` | Batch countdown | Yes |
| POST | `/spot/price_orders` | Create trigger order | Yes |
| GET | `/spot/price_orders` | List trigger orders | Yes |
| DELETE | `/spot/price_orders` | Cancel all triggers | Yes |
| GET | `/spot/price_orders/{order_id}` | Get trigger order | Yes |
| DELETE | `/spot/price_orders/{order_id}` | Cancel trigger | Yes |

## Futures (Perpetual)

See [05_futures_rest_api.md](05_futures_rest_api.md) for detailed documentation of all 50 endpoints.

**Summary:**

| Category | Endpoints |
|----------|-----------|
| Contracts | List all, Get single |
| Market Data | Order book, Trades, Candlesticks, Premium index, Tickers |
| Funding | Funding rate history, Insurance balance history |
| Stats | Contract stats, Index constituents, Liquidation history, Risk limit tiers |
| Account | Get account, Account book |
| Positions | List, Get, Update margin/leverage/risk limit, Dual mode operations |
| Orders | Create, List, Cancel, Amend, Batch operations, Time range queries |
| Trades | Personal history, Time range queries, Position close history |
| Auto Orders | Create, List, Cancel price-triggered orders |
| Other | Countdown cancel, Fee query, Risk limit table |

## Delivery Futures

Under `/delivery/{settle}/*`. Similar structure to Perpetual Futures:
- Contracts, Order book, Trades, Candlesticks, Tickers
- Accounts, Positions, Orders
- Settlements, Price orders

## Options

Under `/options/*`:
- List underlyings, Expiration dates
- Contracts, Settlements, User settlements
- Order book, Tickers, Underlying candlesticks, Trades
- Accounts, Positions, Orders, Personal trade history

## Earn (Lend & Earn)

Under `/earn/*`:
- Earn currencies, Lend/invest, List lends
- Lend records, Interest records
- Auto-invest plan management

## Collateral Loan

Under `/loan/multi_collateral/*`:
- Multi-collateral loan orders, repayment, mortgage management
- Collateral ratios and health metrics

## Flash Swap

Under `/flash_swap/*`:
- List currency pairs
- Create/list/get orders
- Preview swap

## Copy Trading

Copy trading endpoints.

## Rebate

Under `/rebate/*`:
- Transaction history
- Commission history
- Partner aggregated data

## Account

Under `/account/*`:
- Account detail information
- STP group info
