//! ecd: the cache daemon (`behaviors/ecd-daemon.md`, `ecd-protocol.md`, `ecd-store.md`,
//! `ecd-slots.md`, `ecd-identities.md`). See `implementation/03-ecd-server.md` and
//! `04-ecd-store.md`.
//!
//! Paths, keys and log lines are byte strings: the Go daemon never validates UTF-8.

pub mod daemon;
pub mod goerr;
#[cfg(target_os = "linux")]
mod goerr_table;
#[cfg(target_vendor = "apple")]
#[path = "goerr_table_darwin.rs"]
mod goerr_table;
pub mod gofs;
pub mod gopath;
pub mod identity;
pub mod log;
pub mod peer;
pub mod server;
pub mod slots;
pub mod store;
