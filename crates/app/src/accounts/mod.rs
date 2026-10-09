//! App side of accounts: the shared [`Accounts`] list, the askpass server whose environment the
//! git runner passes to every git, the askpass helper mode of this executable, and the Accounts
//! card on the Settings page.

mod card;

use std::ffi::OsString;
use std::sync::Arc;

use accounts::askpass::{self, AskpassServer};
use accounts::{Accounts, KeyringVault};
use gpui_kit::{App, Global};

pub use card::AccountsCard;

pub struct AccountsState {
    pub accounts: Option<Arc<Accounts>>,
    askpass: Option<AskpassServer>,
}

impl Global for AccountsState {}

/// Loads accounts and starts the askpass server (no keychain access yet: secrets are read only
/// when git asks or the user validates).
pub fn init(cx: &mut App) {
    let accounts = crate::state::config_dir().map(|dir| Accounts::new(&dir, KeyringVault));
    let askpass = accounts.as_ref().and_then(|accounts| {
        let accounts = accounts.clone();
        AskpassServer::start(move |ask| accounts.answer(ask))
            .inspect_err(
                |err| tracing::warn!(%err, "askpass server failed; token accounts disabled"),
            )
            .ok()
    });
    cx.set_global(AccountsState { accounts, askpass });
}

pub fn accounts(cx: &App) -> Option<Arc<Accounts>> {
    cx.try_global::<AccountsState>()?.accounts.clone()
}

/// Variables every git needs so pushes and fetches can use stored tokens.
pub fn askpass_env(cx: &App) -> Vec<(OsString, OsString)> {
    let Some(server) = cx
        .try_global::<AccountsState>()
        .and_then(|s| s.askpass.as_ref())
    else {
        return Vec::new();
    };
    match std::env::current_exe() {
        Ok(exe) => server.env(&exe),
        Err(err) => {
            tracing::warn!(%err, "no executable path for askpass");
            Vec::new()
        }
    }
}

/// When git runs this executable as `GIT_ASKPASS`, answer the prompt and exit before any UI
/// starts. Returns the exit code, or `None` for a normal launch.
pub fn helper_main() -> Option<i32> {
    std::env::var_os(askpass::SOCKET_ENV)?;
    let prompt = std::env::args().nth(1).unwrap_or_default();
    Some(match askpass::ask_app(&prompt) {
        Some(answer) => {
            println!("{}", answer.expose());
            0
        }
        None => 1,
    })
}
