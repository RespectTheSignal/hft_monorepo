// Process-wide tokio runtime for Flipster async tasks (WS feeds, cookie refresher).
// GateOrderManager + strategy pipeline are synchronous; this runtime handles the
// async I/O glue. The runtime lives for the lifetime of the process.

use std::sync::OnceLock;
use tokio::runtime::{Handle, Runtime};

static RT: OnceLock<Runtime> = OnceLock::new();

pub fn handle() -> Handle {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("flipster-rt")
            .build()
            .expect("build flipster tokio runtime")
    })
    .handle()
    .clone()
}
