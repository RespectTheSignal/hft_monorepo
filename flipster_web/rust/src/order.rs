use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Side {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MarginType {
    Isolated,
    Cross,
}

#[derive(Debug, Clone, Copy)]
pub enum OrderType {
    Market,
    Limit,
}

impl OrderType {
    pub fn as_api_str(&self) -> &'static str {
        match self {
            OrderType::Market => "ORDER_TYPE_MARKET",
            OrderType::Limit => "ORDER_TYPE_LIMIT",
        }
    }
}

impl Serialize for OrderType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_api_str())
    }
}

#[derive(Debug, Clone)]
pub struct OrderParams {
    pub side: Side,
    pub price: f64,
    pub amount: f64,
    pub leverage: u32,
    pub margin_type: MarginType,
    pub order_type: OrderType,
}

impl OrderParams {
    pub fn builder() -> OrderParamsBuilder {
        OrderParamsBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct OrderParamsBuilder {
    side: Option<Side>,
    price: Option<f64>,
    amount: Option<f64>,
    leverage: Option<u32>,
    margin_type: Option<MarginType>,
    order_type: Option<OrderType>,
}

impl OrderParamsBuilder {
    pub fn side(mut self, side: Side) -> Self {
        self.side = Some(side);
        self
    }

    pub fn price(mut self, price: f64) -> Self {
        self.price = Some(price);
        self
    }

    pub fn amount(mut self, amount: f64) -> Self {
        self.amount = Some(amount);
        self
    }

    pub fn leverage(mut self, leverage: u32) -> Self {
        self.leverage = Some(leverage);
        self
    }

    pub fn margin_type(mut self, margin_type: MarginType) -> Self {
        self.margin_type = Some(margin_type);
        self
    }

    pub fn order_type(mut self, order_type: OrderType) -> Self {
        self.order_type = Some(order_type);
        self
    }

    pub fn build(self) -> OrderParams {
        OrderParams {
            side: self.side.expect("side is required"),
            price: self.price.expect("price is required"),
            amount: self.amount.expect("amount is required"),
            leverage: self.leverage.unwrap_or(1),
            margin_type: self.margin_type.unwrap_or(MarginType::Isolated),
            order_type: self.order_type.unwrap_or(OrderType::Market),
        }
    }
}

/// The JSON body sent to the Flipster API.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequestBody {
    pub side: Side,
    pub request_id: String,
    pub timestamp: String,
    pub ref_server_timestamp: String,
    pub ref_client_timestamp: String,
    pub leverage: u32,
    pub price: String,
    pub amount: String,
    pub attribution: &'static str,
    pub margin_type: MarginType,
    pub order_type: OrderType,
}

/// Raw API response (shape TBD — keep it loose for now).
#[derive(Debug, Deserialize)]
pub struct OrderResponse {
    #[serde(flatten)]
    pub raw: serde_json::Value,
}
