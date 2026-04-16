//! strategy → drain 내부 채널에 실리는 주문 egress seed.
//!
//! wire/protocol 자체가 아니라 strategy 서비스 내부 payload 이므로 이 crate 안에 둔다.

use hft_protocol::{OrderEgressMeta, WireLevel};

/// drain 이전 단계에서 strategy 가 확정하는 주문 메타 seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderEgressMetaSeed {
    /// strategy 인스턴스 내부 단조 증가 시퀀스.
    pub client_seq: u64,
    /// open / close 구분.
    pub level: WireLevel,
    /// reduce_only 여부.
    pub reduce_only: bool,
    /// strategy tag (`v6` / `v7` / `v8`).
    pub strategy_tag: &'static str,
}

impl OrderEgressMetaSeed {
    /// drain 직전 시각과 symbol 해석 결과를 덧붙여 최종 egress meta 로 승격한다.
    pub fn promote(
        &self,
        origin_ts_ns: u64,
        symbol_id: Option<u32>,
        symbol_idx: Option<u32>,
    ) -> OrderEgressMeta<'static> {
        OrderEgressMeta {
            client_seq: self.client_seq,
            strategy_tag: self.strategy_tag,
            level: self.level,
            reduce_only: self.reduce_only,
            origin_ts_ns,
            symbol_id,
            symbol_idx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_promotes_to_egress_meta_with_origin_only_from_drain() {
        let seed = OrderEgressMetaSeed {
            client_seq: 42,
            level: WireLevel::Close,
            reduce_only: true,
            strategy_tag: "v8",
        };

        let meta = seed.promote(123, Some(7), Some(9));
        assert_eq!(meta.client_seq, 42);
        assert_eq!(meta.level, WireLevel::Close);
        assert!(meta.reduce_only);
        assert_eq!(meta.strategy_tag, "v8");
        assert_eq!(meta.origin_ts_ns, 123);
        assert_eq!(meta.symbol_id, Some(7));
        assert_eq!(meta.symbol_idx, Some(9));
    }
}
