//! Entry point: starts the app and handles CLI subcommands.

mod actions;
mod app_overrides;
mod autostart;
mod bindings;
mod bundle;
mod checks;
mod commands;
mod config;
mod daemon;
#[cfg(feature = "debug-access")]
mod debug;
mod devices;
mod discovery;
mod dock;
mod driver;
mod events;
mod fsutil;
mod gestures;
mod hidpp;
mod hub;
mod i18n;
mod ipc;
mod logfile;
mod macos;
mod paths;
mod permissions;
mod profiles;
mod runloop;
mod scroll;
mod shutdown;
mod spaces;
mod sysprefs;
mod thumb;
mod tray;
mod ui;
mod util;

use clap::{Parser, Subcommand};

rust_i18n::i18n!("locales", fallback = "en");

// `i18n!` reads the locale files without telling cargo, so a change in them
// alone did not rebuild the binary; `include_str!` makes cargo track them.
const _: [&str; 2] = [
    include_str!("../locales/en.yml"),
    include_str!("../locales/pl.yml"),
];

#[derive(Parser)]
#[command(name = "mouse", about = "Logitech driver for macOS")]
struct Cli {
    /// UI language code (e.g. en, pl); overrides config and system
    #[arg(long, global = true)]
    lang: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List detected Logitech devices
    List,
    /// Show protocol version and feature list of the first device
    Info,
    /// Show or set the pointer DPI
    Dpi {
        /// New DPI (omit to show the current value)
        value: Option<u16>,
    },
    /// Show or set SmartShift: free or threshold 1-255
    Smartshift {
        /// free | 1-255 (omit to show the current state)
        value: Option<String>,
    },
    /// Show or set scroll wheel options
    Wheel {
        #[arg(long, value_enum)]
        hires: Option<commands::OnOff>,
        #[arg(long, value_enum)]
        ratchet: Option<commands::OnOff>,
    },
    /// Show or set thumb wheel options
    Thumbwheel {
        #[arg(long, value_enum)]
        invert: Option<commands::OnOff>,
        /// Divert thumb wheel events to software
        #[arg(long, value_enum)]
        divert: Option<commands::OnOff>,
    },
    /// Run a button action once, e.g. mission_control or shortcut:cmd+shift+4
    TestAction {
        /// Action name (see config.example.toml)
        name: String,
    },
    /// Run buttons and gestures from the config until Ctrl-C
    Buttons,
    /// Apply the [scroll] settings (mouse wheel direction, speed, smoothing) until Ctrl-C
    Scroll,
    /// Daemon: apply the config, keep it applied after sleep or reconnect, reload on change
    Run,
    /// Show or switch the Easy-Switch channel
    Host {
        /// Channel number, 1-based (omit to show the current one)
        channel: Option<u8>,
    },
    /// Test build: look into the running daemon and menu bar app
    #[cfg(feature = "debug-access")]
    Debug {
        #[command(subcommand)]
        command: debug::cli::DebugCommand,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let role = match cli.command {
        None => std::env::current_exe().ok().and_then(|p| bundle::role(&p)),
        Some(_) => None,
    };
    // Commands that run until Ctrl-C: the signals must be blocked before
    // any thread starts (hidapi starts one per opened device).
    if matches!(cli.command, Some(Command::Buttons | Command::Scroll | Command::Run))
        || role == Some(bundle::Role::Agent)
    {
        shutdown::install_signal_handlers();
    }
    // Started from Finder, `open` or by the settings app there is no
    // terminal: write the output to a log file instead, with more detail. A
    // terminal gets warnings and errors by default; RUST_LOG=info or debug
    // shows more.
    let log = match (&cli.command, role) {
        (None, Some(bundle::Role::App)) => Some(logfile::Log::App),
        (None, Some(bundle::Role::Agent)) | (Some(Command::Run), _) => Some(logfile::Log::Daemon),
        _ => None,
    };
    let default_filter = if log.is_some_and(logfile::redirect) { "info" } else { "warn" };
    #[cfg(not(feature = "debug-access"))]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .format_timestamp_millis()
        .init();
    #[cfg(feature = "debug-access")]
    debug::init_logger(default_filter);
    i18n::init(cli.lang.as_deref());
    match cli.command {
        Some(Command::List) => discovery::print_list(),
        Some(Command::Info) => hidpp::device::print_info(),
        Some(Command::Dpi { value }) => commands::dpi(value),
        Some(Command::Smartshift { value }) => commands::smartshift(value),
        Some(Command::Wheel { hires, ratchet }) => commands::wheel(hires, ratchet),
        Some(Command::Thumbwheel { invert, divert }) => commands::thumbwheel(invert, divert),
        Some(Command::TestAction { name }) => actions::test_action(&name),
        Some(Command::Buttons) => events::run(),
        Some(Command::Scroll) => scroll::run(),
        Some(Command::Run) => daemon::run(),
        Some(Command::Host { channel }) => commands::host(channel),
        #[cfg(feature = "debug-access")]
        Some(Command::Debug { command }) => debug::cli::run(command),
        // Finder / `open` start a bundle without arguments; its executable
        // name says what to run.
        None => match role {
            Some(bundle::Role::Agent) => daemon::run(),
            Some(bundle::Role::App) => tray::run(),
            None => Ok(()),
        },
    }
}
