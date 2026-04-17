//! Prometheus text exposition renderer.

use std::fmt::Write as _;

use hft_time::Stage;

use crate::{counters_snapshot, dump_hdr, gauges_snapshot};

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::ExchangeServer => "exchange_server",
        Stage::WsReceived => "ws_received",
        Stage::Serialized => "serialized",
        Stage::Pushed => "pushed",
        Stage::Published => "published",
        Stage::Subscribed => "subscribed",
        Stage::Consumed => "consumed",
    }
}

/// counters + gauges + HDR p50/p99/p999 를 Prometheus text exposition format 으로 렌더링한다.
pub fn render_prometheus() -> String {
    let mut out = String::new();

    let mut counters = counters_snapshot();
    counters.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in counters {
        let metric = format!("hft_{name}_total");
        let _ = writeln!(out, "# TYPE {metric} counter");
        let _ = writeln!(out, "{metric} {value}");
        let _ = writeln!(out);
    }

    let mut gauges = gauges_snapshot();
    gauges.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in gauges {
        let metric = format!("hft_{name}");
        let _ = writeln!(out, "# TYPE {metric} gauge");
        let _ = writeln!(out, "{metric} {value}");
        let _ = writeln!(out);
    }

    for (stage, snap) in dump_hdr() {
        let stage_name = stage_name(stage);
        for (suffix, value) in [("p50", snap.p50), ("p99", snap.p99), ("p999", snap.p999)] {
            let metric = format!("hft_stage_{stage_name}_{suffix}_ns");
            let _ = writeln!(out, "# TYPE {metric} gauge");
            let _ = writeln!(out, "{metric} {value}");
            let _ = writeln!(out);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::render_prometheus;
    use crate::{
        counter_inc, gauge_set, record_stage_nanos, reset_hdr_for_test, CounterKey, GaugeKey,
    };
    use hft_time::Stage;

    #[test]
    fn render_includes_counters() {
        counter_inc(CounterKey::ZmqDropped);
        let body = render_prometheus();
        assert!(body.contains("hft_zmq_dropped_total"));
    }

    #[test]
    fn render_includes_gauges() {
        gauge_set(GaugeKey::UptimeSeconds, 42);
        let body = render_prometheus();
        assert!(body.contains("hft_uptime_seconds 42"));
    }

    #[test]
    fn render_includes_hdr() {
        reset_hdr_for_test();
        record_stage_nanos(Stage::ExchangeServer, 123);
        let body = render_prometheus();
        assert!(body.contains("hft_stage_exchange_server_p50_ns"));
    }
}
