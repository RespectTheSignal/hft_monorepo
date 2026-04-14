# Binance USDⓈ-M Futures API Documentation

Binance USDⓈ-M 선물 API 문서 정리. 코드 작성 시 참조용.

## 문서 구조

| # | 파일 | 내용 |
|---|------|------|
| 00 | [General Info](00_general_info.md) | Base URL, 인증, 서명, Rate Limit, HTTP 코드 |
| 01 | [WebSocket API General](01_websocket_api_general.md) | WebSocket API 연결, 인증, 요청/응답 형식 |
| 02 | [Market Data REST API](02_market_data_rest_api.md) | 시장 데이터 REST 엔드포인트 34개 (OrderBook, Ticker, Kline 등) |
| 03 | [Trade REST API](03_trade_rest_api.md) | 주문/포지션 관련 REST 엔드포인트 20개 |
| 04 | [Account REST API](04_account_rest_api.md) | 계정 정보, 잔고, 설정 REST 엔드포인트 22개 |
| 05 | [WebSocket Market Streams](05_websocket_market_streams.md) | 실시간 시장 데이터 스트림 20종 (BookTicker, Depth 등) |
| 06 | [WebSocket API Trade/Account](06_websocket_api_trade_account.md) | WebSocket API를 통한 주문 및 계정 조회 |
| 07 | [User Data Streams](07_user_data_streams.md) | Listen Key 관리, 계정 이벤트 (주문, 잔고, 포지션 업데이트) |
| 09 | [Error Codes](09_error_codes.md) | 전체 에러 코드 참조 |

## HFT BookTicker 수신 핵심 참고

- BookTicker 실시간 스트림: `<symbol>@bookTicker` (Real-time, 05번 문서 참조)
- 전체 BookTicker 스트림: `!bookTicker` (5초 간격)
- WebSocket 연결: `wss://fstream.binance.com/ws/<symbol>@bookTicker`
- 복합 스트림: `wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker`
- 연결당 최대 1024 스트림, 24시간 연결 수명

## 주요 Base URL

| 용도 | URL |
|------|-----|
| REST API | `https://fapi.binance.com` |
| WebSocket Streams | `wss://fstream.binance.com` |
| WebSocket API | `wss://ws-fapi.binance.com/ws-fapi/v1` |
