use byteorder::{LittleEndian, WriteBytesExt};
use tracing::info;

/// FlipsterBookTicker binary layout (104 bytes, little-endian)
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

pub struct ZmqPublisher {
    tx: std::sync::mpsc::SyncSender<(String, [u8; BOOKTICKER_SIZE])>,
    msg_count: u64,
}

impl ZmqPublisher {
    pub fn new(addr: &str) -> Self {
        let addr = addr.to_string();
        let (tx, rx) = std::sync::mpsc::sync_channel::<(String, [u8; BOOKTICKER_SIZE])>(65536);

        std::thread::spawn(move || {
            zmq_pub_thread(&addr, rx);
        });

        Self { tx, msg_count: 0 }
    }

    pub fn publish(
        &mut self,
        symbol: &str,
        bid_price: f64,
        ask_price: f64,
        last_price: f64,
        mark_price: f64,
        index_price: f64,
        server_ts_ns: i64,
        publisher_recv_ns: i64,
    ) {
        let sent_ns = crate::time_sync::local_now_ns();
        let mut buf = [0u8; BOOKTICKER_SIZE];

        // symbol (0..32)
        let sym_bytes = symbol.as_bytes();
        let len = sym_bytes.len().min(31);
        buf[..len].copy_from_slice(&sym_bytes[..len]);

        // prices (32..72)
        let mut cursor = &mut buf[32..] as &mut [u8];
        let _ = cursor.write_f64::<LittleEndian>(bid_price);
        let _ = cursor.write_f64::<LittleEndian>(ask_price);
        let _ = cursor.write_f64::<LittleEndian>(last_price);
        let _ = cursor.write_f64::<LittleEndian>(mark_price);
        let _ = cursor.write_f64::<LittleEndian>(index_price);

        // timestamps (72..104)
        let _ = cursor.write_i64::<LittleEndian>(server_ts_ns);
        let _ = cursor.write_i64::<LittleEndian>(publisher_recv_ns);
        let _ = cursor.write_i64::<LittleEndian>(sent_ns);
        // subscriber_recv_ns left as 0 — filled by subscriber

        let topic = format!("flipster_bookticker_{symbol}");
        if self.tx.try_send((topic, buf)).is_ok() {
            self.msg_count += 1;
        }
    }

    pub fn msg_count(&self) -> u64 {
        self.msg_count
    }
}

fn zmq_pub_thread(
    addr: &str,
    rx: std::sync::mpsc::Receiver<(String, [u8; BOOKTICKER_SIZE])>,
) {
    let ctx = zmq::Context::new();
    let pub_sock = ctx.socket(zmq::PUB).expect("ZMQ PUB socket");
    pub_sock.set_sndhwm(100_000).expect("set sndhwm");
    pub_sock.set_sndbuf(4 * 1024 * 1024).expect("set sndbuf");
    pub_sock.set_linger(0).expect("set linger");
    pub_sock.bind(addr).expect("ZMQ PUB bind");

    info!("ZMQ PUB bound to {addr}");

    let mut published: u64 = 0;
    let mut last_log = std::time::Instant::now();

    for (topic, payload) in rx.iter() {
        let _ = pub_sock.send(topic.as_bytes(), zmq::SNDMORE);
        let _ = pub_sock.send(&payload[..], 0);
        published += 1;

        if last_log.elapsed().as_secs() >= 5 {
            info!("ZMQ PUB: {published} messages published");
            last_log = std::time::Instant::now();
        }
    }

    info!("ZMQ PUB thread exiting ({published} total)");
}
