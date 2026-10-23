//! Test build only (Cargo feature `debug-access`, `scripts/bundle.sh
//! --debug`): `mouse debug ...` lets an agent without Input Monitoring,
//! Accessibility or screen recording look into the running daemon (over
//! `agent.sock`) and the menu bar app (over `app.sock`). Unix sockets in the
//! settings folder, readable by their owner only; nothing goes over a network.

pub mod cli;
pub mod daemon;
pub mod menu;
pub mod probe;
pub mod proto;
pub mod server;
pub mod window;

use log::LevelFilter;

/// Sets up logging so `debug log` can raise the level later: the logger
/// itself passes everything (module filters from `RUST_LOG` still apply),
/// the global maximum keeps the level the process starts with.
pub fn init_logger(default_filter: &str) {
    let env = || env_logger::Env::default().default_filter_or(default_filter);
    let base = env_logger::Builder::from_env(env()).build().filter();
    let logger = env_logger::Builder::from_env(env())
        .format_timestamp_millis()
        .filter_level(LevelFilter::Trace)
        .build();
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(base);
        probe::set_base_level(base);
    }
}
