//! Peer authentication (`behaviors/jigd-daemon.md` §Peer authentication; Q-DAEMON-7, Q-DAEMON-8
//! kept; `implementation/03-jigd-server.md` §Peer authentication).
//!
//! On Linux a connection is served iff its `SO_PEERCRED` uid is the daemon's real uid or root,
//! or its primary gid is the `nixbld` group's, or its uid is in nix's default auto-allocated
//! range. Elsewhere every peer is allowed, as in `peer_other.go`.
//!
//! The legacy daemon is built with cgo (go1.25.9, `CGO_ENABLED=1`), so `user.LookupGroup` goes
//! through NSS: this port calls `getgrnam_r` the way Go's cgo resolver does, growing the buffer
//! on `ERANGE` up to 1 MiB.

use std::os::fd::AsFd;

/// nix.conf `start-id` default (`0x34000000`).
pub const AUTO_UIDS: u32 = 872_415_232;
/// nix.conf `id-count` default (`128 << 16`).
pub const AUTO_COUNT: u32 = 128 << 16;

/// What `allowed` compares the peer against, fixed at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Auth {
    /// `os.Getuid()`
    pub uid: u32,
    /// the nixbld gid, -1 if there is no such group
    pub nixbld: i64,
}

impl Auth {
    /// The daemon's real uid and the nixbld group as NSS knows it now (Q-DAEMON-7: looked up
    /// once).
    pub fn from_system() -> Auth {
        Auth {
            uid: rustix::process::getuid().as_raw(),
            nixbld: nixbld(),
        }
    }

    /// The rule of `allowed`, on the peer's uid and primary gid.
    pub fn allows(&self, uid: u32, gid: u32) -> bool {
        uid == self.uid
            || uid == 0
            || (self.nixbld >= 0 && i64::from(gid) == self.nixbld)
            || (AUTO_UIDS..AUTO_UIDS + AUTO_COUNT).contains(&uid)
    }

    /// `allowed(conn)`: a failed `SO_PEERCRED` refuses the peer.
    #[cfg(target_os = "linux")]
    pub fn allowed(&self, conn: impl AsFd) -> bool {
        match rustix::net::sockopt::socket_peercred(conn) {
            Ok(cred) => self.allows(cred.uid.as_raw(), cred.gid.as_raw()),
            Err(_) => false,
        }
    }

    /// `peer_other.go`: every peer (Q-DAEMON-8).
    #[cfg(not(target_os = "linux"))]
    pub fn allowed(&self, _conn: impl AsFd) -> bool {
        true
    }
}

/// `user.LookupGroup("nixbld")` with cgo, then `strconv.Atoi(g.Gid)`: the gid, or -1 when the
/// lookup fails or finds nothing. (cgo formats the gid itself, so the `Atoi` cannot fail here.)
#[allow(unsafe_code)]
fn nixbld() -> i64 {
    const MAX_BUFFER: usize = 1 << 20;
    // SAFETY: sysconf has no preconditions.
    let initial = unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) };
    let mut size = match usize::try_from(initial) {
        Ok(n) if n > 0 && n <= MAX_BUFFER => n,
        // Go: -1 gives 1024, any other unreasonable size the maximum
        _ if initial == -1 => 1024,
        _ => MAX_BUFFER,
    };
    loop {
        let mut buf = vec![0u8; size];
        // SAFETY: `libc::group` is a plain C struct for which all-zero bytes are valid.
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::group = std::ptr::null_mut();
        // SAFETY: the name is NUL-terminated, `grp` and `result` are valid for writes, and `buf`
        // is valid for `size` bytes for the duration of the call; the strings it fills in are
        // not read afterwards.
        let rc = unsafe {
            libc::getgrnam_r(
                c"nixbld".as_ptr(),
                &raw mut grp,
                buf.as_mut_ptr().cast(),
                size,
                &raw mut result,
            )
        };
        if rc == 0 {
            return if result.is_null() {
                -1
            } else {
                i64::from(grp.gr_gid)
            };
        }
        if rc != libc::ERANGE || size * 2 > MAX_BUFFER {
            return -1;
        }
        size *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let a = Auth {
            uid: 1000,
            nixbld: 30000,
        };
        assert!(a.allows(1000, 100));
        assert!(a.allows(0, 100));
        assert!(a.allows(30001, 30000));
        assert!(!a.allows(30001, 100));
        assert!(a.allows(AUTO_UIDS, 5));
        assert!(a.allows(AUTO_UIDS + AUTO_COUNT - 1, 5));
        assert!(!a.allows(AUTO_UIDS + AUTO_COUNT, 5));
        assert!(!a.allows(AUTO_UIDS - 1, 5));
        let none = Auth {
            uid: 1000,
            nixbld: -1,
        };
        assert!(!none.allows(1001, u32::MAX));
        assert!(!none.allows(1001, 0));
    }

    #[test]
    fn own_uid_is_allowed() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(Auth::from_system().allowed(&a));
    }
}
