pub mod client;
pub mod error;
pub mod order;

pub use client::{GateClient, GateConfig};
pub use error::GateError;
pub use order::{OrderParams, OrderResponse, OrderType, Side, TimeInForce};
