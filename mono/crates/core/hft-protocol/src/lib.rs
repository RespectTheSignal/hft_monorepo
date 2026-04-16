//! hft-protocol — 와이어 프로토콜
//!
//! # 내용
//! - [`wire`]: C-struct 120B / 128B 고정 레이아웃 (downstream C/C++/Python 호환)
//! - [`topics`]: ZMQ 토픽 네이밍 규약
//! - [`frame`]: 메시지 프레이밍 `[type_len:u8][type:str][payload:bytes]`
//!
//! # 호환성
//! 레거시 `gate_hft/data_publisher_rust/src/serializer.rs` 와 **byte-exact parity**.
//! 기존 C/Python downstream 이 바로 붙을 수 있도록 한 바이트도 바뀌면 안 된다.
//! `wire::tests::legacy_byte_parity_*` 가 이것을 보장.
//!
//! # re-export
//! 자주 쓰는 상수/함수는 루트에서 바로 접근 가능하도록 re-export 한다.

#![deny(rust_2018_idioms)]

pub mod frame;
pub mod order_wire;
pub mod topics;
pub mod wire;

// ── 자주 쓰는 심볼 top-level re-export ────────────────────────────────────────

pub use frame::{create_message, parse_frame, write_frame_into, FrameError, FrameView};
pub use topics::{
    parse_topic, ParsedTopic, TopicBuilder, MSG_BOOKTICKER, MSG_TRADE, MSG_WEBBOOKTICKER,
    TOPIC_SEP,
};
pub use wire::{
    decode_bookticker, decode_trade, encode_bookticker_into, encode_trade_into,
    patch_bookticker_pushed_ms, patch_trade_pushed_ms, BookTickerWire, TradeWire, WireError,
    BOOKTICKER_PUBLISHER_SENT_OFFSET, BOOK_TICKER_SIZE, EXCHANGE_FIELD_LEN, SYMBOL_FIELD_LEN,
    TRADE_PUBLISHER_SENT_OFFSET, TRADE_SIZE,
};
