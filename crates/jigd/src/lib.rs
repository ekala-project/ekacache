//! jigd: the cache daemon (`behaviors/jigd-daemon.md`, `jigd-protocol.md`, `jigd-store.md`,
//! `jigd-slots.md`, `jigd-identities.md`). See `implementation/03-jigd-server.md` and
//! `04-jigd-store.md`.
//!
//! Paths, keys and log lines are byte strings: the Go daemon never validates UTF-8.

pub mod daemon;
pub mod goerr;
mod goerr_table;
pub mod gofs;
pub mod gopath;
pub mod identity;
pub mod log;
pub mod peer;
pub mod server;
pub mod slots;
pub mod store;
