//! Requests from the control socket and commands from the settings app.

use rust_i18n::t;

use super::Shared;
use super::watch::load_checked;
use crate::hub::Command;
use crate::ipc::{AgentInfo, Reply, Request, Status};
use crate::permissions;
use crate::profiles::{self, Store};
use crate::scroll;
use crate::shutdown;

/// Answers one request from the control socket.
pub(super) fn answer(req: Request, shared: &Shared) -> Reply {
    let hub = &shared.hub;
    match req {
        Request::Status => Reply::Status(Status {
            mice: hub.mice(),
            missing: permissions::missing(),
            scroll_problem: scroll::problem(),
            profile: hub.profile(),
            agent: Some(AgentInfo::current()),
        }),
        Request::Dpi(v) => {
            hub.push_command(Command::SetDpi(v));
            Reply::Ok
        }
        Request::Reload => {
            hub.push_command(Command::Reload);
            Reply::Ok
        }
        Request::Quit => {
            shutdown::request_stop();
            Reply::Ok
        }
        #[cfg(feature = "debug-access")]
        Request::Debug(d) => crate::debug::daemon::answer(d, hub, &shared.snapshot().1),
    }
}

/// Applies a command from the settings app. DPI is saved to the current
/// settings.
pub(super) fn handle_command(cmd: Command, store: &Store, shared: &Shared) {
    let loaded = match cmd {
        Command::SetDpi(v) => match store.update_current(|c| c.dpi = Some(v)) {
            Ok(()) => {
                println!("{}", t!("daemon.config_saved", value = v));
                load_checked(store)
            }
            Err(e) => Err(anyhow::anyhow!(e)),
        },
        Command::Reload => load_checked(store),
    };
    match loaded {
        Ok((name, c)) if cmd == Command::Reload => {
            shared.hub.set_profile(&name);
            shared.reapply(c);
            reloaded(&name);
        }
        Ok((name, c)) => {
            shared.hub.set_profile(&name);
            if shared.replace(c) {
                reloaded(&name);
            }
        }
        Err(e) => log::warn!("{}", t!("daemon.reload_failed", error = e)),
    }
}

/// Reports new settings and lets the settings app show the (maybe new) profile.
pub(super) fn reloaded(profile: &str) {
    println!("{}", t!("daemon.reloaded", name = profiles::display_name(profile)));
}
