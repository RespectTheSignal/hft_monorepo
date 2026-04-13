use byteorder::{LittleEndian, ReadBytesExt};
use std::io::Cursor;

/// FlipsterBookTicker binary layout (104 bytes, little-endian).
/// Shared contract between publisher and subscriber.
///
/// | Field               | Offset | Size | Type     |
/// |---------------------|--------|------|----------|
/// | symbol              | 0      | 32   | [u8;32]  |
/// | bid_price           | 32     | 8    | f64 LE   |
/// | ask_price           | 40     | 8    | f64 LE   |
/// | last_price          | 48     | 8    | f64 LE   |
/// | mark_price          | 56     | 8    | f64 LE   |
/// | index_price         | 64     | 8    | f64 LE   |
/// | server_ts_ns        | 72     | 8    | i64 LE   |
/// | publisher_recv_ns   | 80     | 8    | i64 LE   |
/// | publisher_sent_ns   | 88     | 8    | i64 LE   |
/// | subscriber_recv_ns  | 96     | 8    | i64 LE   |
pub const BOOKTICKER_SIZE: usize = 104;

/// Subscriber stamps its receive timestamp at this offset.
pub const SUBSCRIBER_RECV_NS_OFFSET: usize = 96;

#[derive(Debug, Clone)]
pub struct FlipsterBookTicker {
    pub symbol: String,
    pub bid_price: f64,
    pub ask_price: f64,
    pub last_price: f64,
    pub mark_price: f64,
    pub index_price: f64,
    pub server_ts_ns: i64,
    pub publisher_recv_ns: i64,
    pub publisher_sent_ns: i64,
    pub subscriber_recv_ns: i64,
}

impl FlipsterBookTicker {
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < BOOKTICKER_SIZE {
            return None;
        }

        // symbol: 0..32 (null-terminated)
        let symbol_end = buf[..32].iter().position(|&b| b == 0).unwrap_or(32);
        let symbol = std::str::from_utf8(&buf[..symbol_end]).ok()?.to_string();

        let mut cur = Cursor::new(&buf[32..]);
        let bid_price = cur.read_f64::<LittleEndian>().ok()?;
        let ask_price = cur.read_f64::<LittleEndian>().ok()?;
        let last_price = cur.read_f64::<LittleEndian>().ok()?;
        let mark_price = cur.read_f64::<LittleEndian>().ok()?;
        let index_price = cur.read_f64::<LittleEndian>().ok()?;
        let server_ts_ns = cur.read_i64::<LittleEndian>().ok()?;
        let publisher_recv_ns = cur.read_i64::<LittleEndian>().ok()?;
        let publisher_sent_ns = cur.read_i64::<LittleEndian>().ok()?;
        let subscriber_recv_ns = cur.read_i64::<LittleEndian>().ok()?;

        Some(Self {
            symbol,
            bid_price,
            ask_price,
            last_price,
            mark_price,
            index_price,
            server_ts_ns,
            publisher_recv_ns,
            publisher_sent_ns,
            subscriber_recv_ns,
        })
    }
}

/// Parse topic string: "flipster_bookticker_{SYMBOL}" → ("flipster", "bookticker", "{SYMBOL}")
pub fn parse_topic(topic: &[u8]) -> Option<(&str, &str, &str)> {
    let s = std::str::from_utf8(topic).ok()?;
    let mut parts = s.splitn(3, '_');
    let exchange = parts.next()?;
    let data_type = parts.next()?;
    let symbol = parts.next()?;
    Some((exchange, data_type, symbol))
}

/// Stamp subscriber_recv_ns in-place at offset 88 in the payload.
pub fn stamp_recv_ns(payload: &mut [u8], recv_ns: i64) {
    if payload.len() >= BOOKTICKER_SIZE {
        payload[SUBSCRIBER_RECV_NS_OFFSET..SUBSCRIBER_RECV_NS_OFFSET + 8]
            .copy_from_slice(&recv_ns.to_le_bytes());
    }
}
