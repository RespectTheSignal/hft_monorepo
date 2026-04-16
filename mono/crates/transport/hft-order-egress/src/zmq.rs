//! ZMQ fallback 기반 order egress 구현.

use std::sync::Mutex;

use hft_config::{order_egress::ZmqOrderEgressConfig, ZmqConfig};
use hft_exchange_api::OrderRequest;
use hft_protocol::{
    order_request_to_order_request_wire, order_wire::ORDER_REQUEST_WIRE_SIZE, OrderEgressMeta,
};
use hft_telemetry::{counter_inc, CounterKey};
use hft_zmq::{Context, PushSocket, SendOutcome, ZmqResult, ZmqWrapError};

use crate::{OrderEgress, SubmitError, SubmitOutcome};

/// ZMQ PUSH socket 으로 raw 128B order wire 를 보내는 구현체.
pub struct ZmqOrderEgress {
    socket: Mutex<PushSocket>,
}

impl ZmqOrderEgress {
    /// 프로세스 로컬 context 를 사용해 PUSH socket 을 연다.
    pub fn connect(cfg: &ZmqOrderEgressConfig) -> ZmqResult<Self> {
        Self::connect_with_context(Context::new(), cfg)
    }

    /// 외부 context 를 주입받아 PUSH socket 을 연다.
    ///
    /// 테스트의 `inproc://` endpoint 는 같은 context 를 공유해야 하므로 이 생성자를 쓴다.
    pub fn connect_with_context(ctx: Context, cfg: &ZmqOrderEgressConfig) -> ZmqResult<Self> {
        let common = ZmqConfig {
            hwm: cfg.send_hwm,
            linger_ms: cfg.linger_ms,
            drain_batch_cap: 1,
            push_endpoint: String::new(),
            pub_endpoint: String::new(),
            sub_endpoint: String::new(),
            order_ingress_bind: None,
        };
        let socket = ctx.push_with_reconnect(
            &cfg.endpoint,
            &common,
            cfg.reconnect_interval_ms,
            cfg.reconnect_interval_max_ms,
        )?;
        Ok(Self {
            socket: Mutex::new(socket),
        })
    }
}

impl OrderEgress for ZmqOrderEgress {
    fn try_submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        let wire = match order_request_to_order_request_wire(req, meta) {
            Ok(wire) => wire,
            Err(e) => {
                counter_inc(CounterKey::OrderEgressSerializeError);
                return Err(SubmitError::Adapt(e));
            }
        };

        let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
        wire.encode(&mut buf);

        let socket = self.socket.lock().expect("zmq socket lock poisoned");
        match socket.send_bytes_dontwait(&buf) {
            SendOutcome::Sent => {
                counter_inc(CounterKey::OrderEgressZmqOk);
                Ok(SubmitOutcome::Sent)
            }
            SendOutcome::WouldBlock => Ok(SubmitOutcome::WouldBlock),
            SendOutcome::Error(e) => Err(SubmitError::Transport(e.to_string())),
        }
    }
}

impl From<ZmqWrapError> for SubmitError {
    fn from(value: ZmqWrapError) -> Self {
        Self::Transport(value.to_string())
    }
}
