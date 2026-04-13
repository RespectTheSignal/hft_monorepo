use byteorder::{LittleEndian, ReadBytesExt};
use std::io::Cursor;

pub const BOOKTICKER_SIZE: usize = 104;

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
