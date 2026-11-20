//! The job-slot guard (`behaviors/ec-cache-client.md §Slot guard`, `process.cc:29-49`).
//!
//! Legacy's RAII `Slot` releases the slot in its destructor. The adopted ordering (Q-CC-2 /
//! Q-CLIENT-2: DONE after the PUTs) needs the cache connection while the slot is held, so the
//! guard here is a scope: `hold` takes the slot, runs the closure with the client and the
//! environment, and releases the slot after the closure returns.

use crate::client::Client;
use crate::process::ProcEnv;

/// Take a slot (if needed and possible), run `f`, release it.
///
/// Skipped when `EKACACHE_SLOT` is non-empty (a parent ec holds one). Connects when not connected.
/// The build name is `$NIX_BUILD_TOP` if set (even empty, Q-CLIENT-3), else `-`. While the slot
/// is held, `EKACACHE_SLOT=1` is in ec's environment, so every child inherits it; it is removed
/// again afterwards. A failed `SLOT` means "run without a slot" and leaves `EKACACHE_SLOT` alone.
pub fn hold<R>(
    client: &mut Client,
    socket: &[u8],
    env: &mut ProcEnv,
    f: impl FnOnce(&mut Client, &mut ProcEnv) -> R,
) -> R {
    let build = acquire(client, socket, env);
    let r = f(client, env);
    if let Some(build) = build {
        client.release_slot(&build);
        env.remove("EKACACHE_SLOT");
    }
    r
}

fn acquire(client: &mut Client, socket: &[u8], env: &mut ProcEnv) -> Option<Vec<u8>> {
    if env.non_empty("EKACACHE_SLOT").is_some() {
        return None;
    }
    if !client.connected() && !client.connect(socket) {
        return None;
    }
    let build = env.get_or("NIX_BUILD_TOP", b"-").to_vec();
    if client.acquire_slot(&build) {
        env.set("EKACACHE_SLOT", "1");
        Some(build)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ekacache_core::env::Env;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn sets_and_clears_ec_slot() {
        let (a, mut s) = UnixStream::pair().unwrap();
        let mut c = Client::from_fd(OwnedFd::from(a), 1);
        let mut env = ProcEnv::new(Env::from_pairs(&[("NIX_BUILD_TOP", "/build")]));
        s.write_all(b"OK\nOK\n").unwrap();
        let seen = hold(&mut c, b"", &mut env, |_, env| {
            env.get("EKACACHE_SLOT").map(<[u8]>::to_vec)
        });
        assert_eq!(seen.as_deref(), Some(&b"1"[..]));
        assert_eq!(env.get("EKACACHE_SLOT"), None);
        drop(c);
        let mut sent = Vec::new();
        s.read_to_end(&mut sent).unwrap();
        assert_eq!(sent, b"SLOT /build\nDONE /build\n");
    }

    #[test]
    fn inherited_slot_and_refusal() {
        let (a, mut s) = UnixStream::pair().unwrap();
        let mut c = Client::from_fd(OwnedFd::from(a), 1);
        let mut env = ProcEnv::new(Env::from_pairs(&[("EKACACHE_SLOT", "1")]));
        hold(&mut c, b"", &mut env, |_, _| ());
        assert_eq!(env.get("EKACACHE_SLOT"), Some(&b"1"[..]));
        // set but empty: a slot is requested; `-` without NIX_BUILD_TOP; a refusal leaves it
        let mut env = ProcEnv::new(Env::from_pairs(&[("EKACACHE_SLOT", "")]));
        s.write_all(b"NO\n").unwrap();
        hold(&mut c, b"", &mut env, |_, _| ());
        assert_eq!(env.get("EKACACHE_SLOT"), Some(&b""[..]));
        drop(c);
        let mut sent = Vec::new();
        s.read_to_end(&mut sent).unwrap();
        assert_eq!(sent, b"SLOT -\n");
    }

    #[test]
    fn no_daemon_runs_anyway() {
        let mut c = Client::new();
        let mut env = ProcEnv::new(Env::default());
        assert_eq!(hold(&mut c, b"/nonexistent", &mut env, |_, _| 7), 7);
        assert_eq!(env.get("EKACACHE_SLOT"), None);
    }
}
