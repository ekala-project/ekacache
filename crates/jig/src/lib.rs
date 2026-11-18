//! jig: the compiler launcher (`behaviors/jig-dispatch.md` and the mode documents,
//! `implementation/05-jig-foundation.md` onwards).
//!
//! jig is synchronous and single-threaded. Everything a mode needs from the process (arguments,
//! environment, store configuration, socket path) is in `Ctx`, captured once in `main`.

pub mod cache_cmd;
pub mod cc;
pub mod client;
pub mod dispatch;
pub mod driver;
pub mod fixup;
pub mod gocache;
pub mod launcher;
pub mod log;
pub mod nix_store;
pub mod process;
pub mod rustc;
pub mod slot;
pub mod sys;
pub mod twokey;
pub mod util;

use jig_core::env::Env;
use jig_core::fspath;
use jig_core::store_path::{Store, default_socket};

use crate::process::ProcEnv;

/// The process state every mode reads (`main.cc:93-111`).
pub struct Ctx {
    /// `argv[0]` as given (empty when `argc == 0`).
    pub argv0: Vec<u8>,
    /// jig's environment (`getenv`/`setenv`), inherited by every child.
    pub env: ProcEnv,
    /// `JIG_STORE_IDENTITY`, `out`, `JIG_STORE_ROOTS`, read once.
    pub store: Store,
    /// `$JIG_SOCK` if set (even empty), else `<StateDir>/jigd/socket`.
    pub socket: Vec<u8>,
}

impl Ctx {
    /// The context for `argv0` and an environment.
    #[must_use]
    pub fn new(argv0: Vec<u8>, env: Env) -> Self {
        let store = Store::from_env(&env);
        let socket = env
            .get("JIG_SOCK")
            .map_or_else(default_socket, <[u8]>::to_vec);
        Self {
            argv0,
            env: ProcEnv::new(env),
            store,
            socket,
        }
    }

    /// `self` for `GOCACHEPROG` (`main.cc:109-110`): `std::filesystem::absolute(argv[0])` (made
    /// absolute, not resolved) when `argv[0]` contains `/`, else `/proc/self/exe`'s target; `""`
    /// on error.
    #[must_use]
    pub fn self_path(&self) -> Vec<u8> {
        if self.argv0.contains(&b'/') {
            if fspath::is_absolute(&self.argv0) {
                return self.argv0.clone();
            }
            return std::env::current_dir()
                .ok()
                .and_then(|cwd| {
                    fspath::absolute_in(&self.argv0, cwd.as_os_str().as_encoded_bytes())
                })
                .unwrap_or_default();
        }
        std::fs::read_link("/proc/self/exe")
            .map(|p| p.into_os_string().into_encoded_bytes())
            .unwrap_or_default()
    }
}
