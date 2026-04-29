// C Struct definitions (must match schemas/rust/c_structs.rs)

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BookTickerC {
    pub exchange: [u8; 16],      // "binance", "gate" (padded with 0)
    pub symbol: [u8; 32],        // "BTC_USDT" (padded with 0)
    pub bid_price: f64,
    pub ask_price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
    pub event_time: i64,         // timestamp_ms
    pub server_time: i64,        // timestamp_ms
    pub publisher_sent_ms: i64,
    pub subscriber_received_ms: i64,
    pub subscriber_dump_ms: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TradeC {
    pub exchange: [u8; 16],
    pub symbol: [u8; 32],
    pub price: f64,
    pub size: f64,
    pub id: i64,
    pub create_time: i64,        // seconds
    pub create_time_ms: i64,     // milliseconds
    pub is_internal: bool,
    pub _padding: [u8; 7],       // Align to 8 bytes
    pub server_time: i64,
    pub publisher_sent_ms: i64,
    pub subscriber_received_ms: i64,
    pub subscriber_dump_ms: i64,
}

impl BookTickerC {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < std::mem::size_of::<Self>() {
            return None;
        }
        unsafe {
            Some(std::ptr::read(bytes.as_ptr() as *const Self))
        }
    }
}

impl TradeC {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < std::mem::size_of::<Self>() {
            return None;
        }
        unsafe {
            Some(std::ptr::read(bytes.as_ptr() as *const Self))
        }
    }
}

// Helper functions
impl BookTickerC {
    pub fn symbol_str(&self) -> String {
        let end = self.symbol.iter().position(|&b| b == 0).unwrap_or(self.symbol.len());
        String::from_utf8_lossy(&self.symbol[..end]).to_string()
    }
    
    pub fn exchange_str(&self) -> String {
        let end = self.exchange.iter().position(|&b| b == 0).unwrap_or(self.exchange.len());
        String::from_utf8_lossy(&self.exchange[..end]).to_string()
    }
}

impl TradeC {
    pub fn symbol_str(&self) -> String {
        let end = self.symbol.iter().position(|&b| b == 0).unwrap_or(self.symbol.len());
        String::from_utf8_lossy(&self.symbol[..end]).to_string()
    }
    
    pub fn exchange_str(&self) -> String {
        let end = self.exchange.iter().position(|&b| b == 0).unwrap_or(self.exchange.len());
        String::from_utf8_lossy(&self.exchange[..end]).to_string()
    }
}

