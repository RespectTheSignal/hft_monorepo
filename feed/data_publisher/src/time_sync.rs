use crate::error::Result;
use crate::flipster::rest::FlipsterRestClient;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

/// Current local time as nanoseconds since Unix epoch.
pub fn local_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

/// Estimate clock offset (server minus local) in nanoseconds using an
/// NTP-style approach: for each sample we record t1 (local before), t2
/// (server time from response), t3 (local after), then
///     offset_i = t2 - (t1 + t3) / 2
/// The median of all samples is returned for robustness.
pub async fn estimate_clock_offset(
    client: &FlipsterRestClient,
    samples: usize,
) -> Result<i64> {
    let mut offsets = Vec::with_capacity(samples);
    let mut rtts = Vec::with_capacity(samples);

    for i in 0..samples {
        if i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        let t1 = local_now_ns();
        let server_ns = client.get_server_time().await?;
        let t3 = local_now_ns();

        let rtt = t3 - t1;
        let offset = server_ns - (t1 + t3) / 2;

        info!(
            "time sync sample {}/{}: RTT={:.3}ms  offset={:.3}ms",
            i + 1,
            samples,
            rtt as f64 / 1_000_000.0,
            offset as f64 / 1_000_000.0,
        );

        rtts.push(rtt);
        offsets.push(offset);
    }

    offsets.sort();
    let median = offsets[offsets.len() / 2];

    rtts.sort();
    let median_rtt = rtts[rtts.len() / 2];

    info!(
        "clock offset (median): {:.3}ms   RTT (median): {:.3}ms",
        median as f64 / 1_000_000.0,
        median_rtt as f64 / 1_000_000.0,
    );

    Ok(median)
}
