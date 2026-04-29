// Optional logging: set GATE_HFT_QUIET=1 to disable all stdout/stderr and log crate output.

use std::sync::OnceLock;

static GATE_HFT_QUIET: OnceLock<bool> = OnceLock::new();

/// True when GATE_HFT_QUIET is set (print_* and plog!/elog! should no-op). Cached for resource optimization.
pub fn is_quiet() -> bool {
    *GATE_HFT_QUIET.get_or_init(|| std::env::var("GATE_HFT_QUIET").is_ok())
}

/// Initialize log4rs: when GATE_HFT_QUIET=1, root level is Off; otherwise load log4rs.yaml.
/// Call this right after dotenvy::dotenv() and before any info!/warn! etc.
pub fn init_logging() {
    if is_quiet() {
        use log::LevelFilter;
        use log4rs::append::console::ConsoleAppender;
        use log4rs::config::{Appender, Config, Root};
        let stdout = ConsoleAppender::builder().build();
        let config = Config::builder()
            .appender(Appender::builder().build("stdout", Box::new(stdout)))
            .build(Root::builder().appender("stdout").build(LevelFilter::Off))
            .unwrap();
        log4rs::init_config(config).unwrap();
    } else {
        log4rs::init_file("log4rs.yaml", Default::default()).unwrap();
    }
}

#[macro_export]
macro_rules! plog {
    ($($t:tt)*) => {
        if !$crate::log::is_quiet() {
            println!($($t)*)
        }
    };
}

#[macro_export]
macro_rules! elog {
    ($($t:tt)*) => {
        if !$crate::log::is_quiet() {
            eprintln!($($t)*)
        }
    };
}
