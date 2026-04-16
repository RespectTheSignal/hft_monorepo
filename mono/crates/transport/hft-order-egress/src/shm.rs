//! SHM 기반 order egress 구현.

use std::sync::Arc;

use hft_exchange_api::OrderRequest;
use hft_protocol::{order_request_to_order_frame, OrderEgressMeta};
use hft_strategy_shm::StrategyShmClient;
use hft_telemetry::{counter_inc, CounterKey};

use crate::{OrderEgress, SubmitError, SubmitOutcome};

/// Strategy VM 의 local shared-region order ring 으로 주문을 내보내는 구현체.
pub struct ShmOrderEgress {
    client: Arc<StrategyShmClient>,
}

impl ShmOrderEgress {
    /// strategy SHM client 를 감싼 egress 생성.
    pub fn new(client: Arc<StrategyShmClient>) -> Self {
        Self { client }
    }

    /// 내부 client 접근.
    pub fn client(&self) -> &Arc<StrategyShmClient> {
        &self.client
    }
}

impl OrderEgress for ShmOrderEgress {
    fn try_submit(
        &self,
        req: &OrderRequest,
        meta: &OrderEgressMeta<'_>,
    ) -> Result<SubmitOutcome, SubmitError> {
        let symbol_idx = match self.client.symtab().get_or_intern(req.exchange, req.symbol.as_str()) {
            Ok(idx) => idx,
            Err(e) => {
                counter_inc(CounterKey::ShmInternFail);
                return Err(SubmitError::Transport(format!("symtab intern failed: {e}")));
            }
        };

        let mut resolved = *meta;
        resolved.symbol_idx = Some(symbol_idx);

        let frame = match order_request_to_order_frame(req, &resolved) {
            Ok(frame) => frame,
            Err(e) => {
                counter_inc(CounterKey::OrderEgressSerializeError);
                return Err(SubmitError::Adapt(e));
            }
        };

        if self.client.publish_frame(&frame) {
            counter_inc(CounterKey::ShmOrderPublished);
            Ok(SubmitOutcome::Sent)
        } else {
            counter_inc(CounterKey::ShmOrderFullDrop);
            Ok(SubmitOutcome::WouldBlock)
        }
    }
}
