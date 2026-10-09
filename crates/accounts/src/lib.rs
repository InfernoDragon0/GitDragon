//! Accounts: Git Credential Manager, PATs, OAuth; secrets live in the OS keychain.
//!
//! - [`model`]: account metadata and exact host matching.
//! - [`store`]: `accounts.json` (metadata only).
//! - [`vault`]: the keychain (fails closed).
//! - [`secret`]: in-memory tokens that zero themselves and never print.

pub mod askpass;
pub mod credential;
pub mod manager;
pub mod model;
pub mod net;
pub mod oauth;
pub mod secret;
pub mod store;
pub mod validate;
pub mod vault;

pub use manager::{Accounts, AddError, NewAccount};
pub use model::{Account, AuthMode, HostKind, Origin, find_account};
pub use secret::Secret;
pub use store::AccountStore;
pub use vault::{KeyringVault, MemoryVault, Vault, VaultError};
