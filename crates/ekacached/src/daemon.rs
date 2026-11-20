//! The ecd process: command line, environment, startup order, the socket, signals and shutdown
//! (`behaviors/ecd-daemon.md` §Command line, §Environment, §Startup order, §Signals and
//! shutdown, §Exit codes, §Logging; `environment.md` §ecd; `implementation/03-ecd-server.md`
//! §Runtime shape, §Environment and budgets, §Shutdown).
//!
//! Startup is sequential and fatal on error, as in Go: everything up to the `chmod` of the
//! socket runs before the tokio runtime exists. Adopted Q-DAEMON-1: `EKACACHED_SIZE` and
//! `EKACACHED_COLD_SIZE` are parsed strictly (`<n>[M|G|T]`, plain = GiB, 0 = unbounded); an invalid
//! value exits 2 before the store is opened, and each tier's budget is logged after it is.
//! Kept: Q-DAEMON-2 (any one argument is the socket), -3 (whatever is at the path is removed),
//! -4, -5 (any accept error is a clean exit 0), -6 (only the first signal counts), -9, -10
//! (`@` makes an abstract socket and the `chmod` then fails), -11, -13 (the affinity mask sets
//! the default slot count).
//!
//! SIGQUIT keeps the Rust default (terminate with a core), where Go dumps goroutines and exits 2.

use std::ffi::OsString;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

use crate::goerr::GoErr;
use crate::gofs;
use crate::gopath;
use crate::identity::Identities;
use crate::log::Logger;
use crate::peer::Auth;
use crate::server::{self, Ctx};
use crate::slots::Slots;
use crate::store::{Store, StoreConfig, Tier};

/// `usage`, byte for byte (`main.go:162-173`).
pub const USAGE: &str = "usage: ecd <socket>

The compile cache and job-slot daemon ec talks to. Nix builds reach it through
sandbox-paths /nix/var/nix/ekacache/socket=<socket>.

  NIX_STORE_DIR   the only tree IDS answers for (default /nix/store)
  XDG_CACHE_HOME  packs live in $XDG_CACHE_HOME/ekacache/packs (default ~/.cache)
  EKACACHED_SIZE       cache budget in GiB (default 50)
  EKACACHED_COLD       a second, slower pack directory that takes what EKACACHED_SIZE pushes out
  EKACACHED_COLD_SIZE  its budget in GiB (default 0: unbounded)
  EKACACHED_SLOTS      concurrent compile slots (default: number of CPUs)
";

/// Blocking-pool cap: a build with 1 000 parallel PUTs must not start 1 000 threads.
const MAX_BLOCKING_THREADS: usize = 512;

/// An environment variable as bytes; unset and empty are the same.
fn env(name: &str) -> Option<Vec<u8>> {
    std::env::var_os(name)
        .map(|v| v.as_bytes().to_vec())
        .filter(|v| !v.is_empty())
}

/// The adopted budget syntax: `^[0-9]+[MmGgTt]?$`, a plain number in GiB; `None` if invalid or
/// above `i64::MAX` bytes.
pub fn parse_budget(v: &[u8]) -> Option<i64> {
    let (digits, shift) = match v.last() {
        Some(b'M' | b'm') => (&v[..v.len() - 1], 20),
        Some(b'G' | b'g') => (&v[..v.len() - 1], 30),
        Some(b'T' | b't') => (&v[..v.len() - 1], 40),
        _ => (v, 30),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: i64 = 0;
    for &d in digits {
        n = n.checked_mul(10)?.checked_add(i64::from(d - b'0'))?;
    }
    n.checked_mul(1 << shift)
}

/// A budget variable: the default if unset or empty, else the strict syntax or exit 2.
fn budget(name: &str, default: i64) -> i64 {
    let Some(v) = env(name) else {
        return default;
    };
    if let Some(b) = parse_budget(&v) {
        return b;
    }
    let mut msg = format!("ecd: invalid {name} \"").into_bytes();
    msg.extend_from_slice(&v);
    msg.extend_from_slice(b"\": want <n>[M|G|T], 0 = unbounded\n");
    let _ = std::io::stderr().write_all(&msg);
    std::process::exit(2);
}

/// `runtime.NumCPU()`: the CPUs in the affinity mask, 1 if it cannot be read (Q-DAEMON-13).
#[cfg(target_os = "linux")]
pub fn num_cpu() -> i64 {
    match rustix::thread::sched_getaffinity(None) {
        Ok(set) if set.count() > 0 => i64::from(set.count()),
        _ => 1,
    }
}

/// `runtime.NumCPU()` on darwin (`getncpu`, `runtime/os_darwin.go`): `sysctl hw.ncpu` when it
/// succeeds with a positive count, else 1 (`implementation/11-macos.md` §Inventory).
#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
pub fn num_cpu() -> i64 {
    let mut n: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated; `n` is valid for `len` bytes of output; no new value.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.ncpu".as_ptr(),
            (&raw mut n).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc >= 0 && n > 0 { i64::from(n) } else { 1 }
}

/// `log.Fatal(err)`: one log line, exit 1.
fn fatal(log: &Logger, msg: &[u8]) -> ! {
    log.log(msg);
    std::process::exit(1);
}

/// `net.ListenUnix("unix", sock)`: `socket` (non-blocking, close-on-exec), `SO_REUSEADDR`,
/// `bind` with Go's address conversion, `listen` with the `somaxconn` backlog. On Linux a
/// leading `@` is abstract, a name of 108 bytes or more is `EINVAL` and an empty one autobinds.
/// On macOS (`syscall_bsd.go`) `@` is an ordinary path (Q-DAEMON-10 is Linux only), and an
/// empty name or one of 104 bytes or more is `EINVAL`.
#[allow(unsafe_code)]
pub fn listen_unix(sock: &[u8]) -> Result<std::os::unix::net::UnixListener, GoErr> {
    let op = |err: GoErr| GoErr::Op {
        op: "listen",
        net: "unix",
        source: None,
        addr: Some(sock.to_vec()),
        err: Box::new(err),
    };
    let syscall = |name: &'static str| {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        op(GoErr::Syscall { name, errno })
    };
    #[cfg(target_os = "linux")]
    let flags = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    #[cfg(target_vendor = "apple")]
    let flags = libc::SOCK_STREAM;
    // SAFETY: plain syscall with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, flags, 0) };
    if fd < 0 {
        return Err(syscall("socket"));
    }
    // SAFETY: `fd` is a socket we just created and own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Go's darwin `sysSocket`: `socket`, then `CloseOnExec` and `SetNonblock`, whose errors it
    // reports as `setnonblock`
    #[cfg(target_vendor = "apple")]
    {
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).map_err(|e| {
            op(GoErr::Syscall {
                name: "setnonblock",
                errno: e.raw_os_error(),
            })
        })?;
        let fl = rustix::fs::fcntl_getfl(&fd).map_err(|e| {
            op(GoErr::Syscall {
                name: "setnonblock",
                errno: e.raw_os_error(),
            })
        })?;
        rustix::fs::fcntl_setfl(&fd, fl | rustix::fs::OFlags::NONBLOCK).map_err(|e| {
            op(GoErr::Syscall {
                name: "setnonblock",
                errno: e.raw_os_error(),
            })
        })?;
    }
    let one: libc::c_int = 1;
    // SAFETY: `one` is a valid c_int for the duration of the call.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&raw const one).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(syscall("setsockopt"));
    }
    // SAFETY: an all-zero sockaddr_un is valid.
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let cap = sa.sun_path.len();
    #[cfg(target_os = "linux")]
    let bad = sock.len() > cap || (sock.len() == cap && sock.first() != Some(&b'@'));
    #[cfg(target_vendor = "apple")]
    let bad = sock.is_empty() || sock.len() >= cap;
    if bad {
        return Err(op(GoErr::Syscall {
            name: "bind",
            errno: libc::EINVAL,
        }));
    }
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, &s) in sa.sun_path.iter_mut().zip(sock) {
        *d = s as libc::c_char;
    }
    // family, name, NUL
    #[cfg(target_os = "linux")]
    let len = if sock.first() == Some(&b'@') {
        // Q-DAEMON-10: abstract; the leading NUL is the marker, and no trailing NUL is counted
        sa.sun_path[0] = 0;
        2 + sock.len()
    } else {
        2 + if sock.is_empty() { 0 } else { sock.len() + 1 }
    };
    // `sun_len`, family, name, NUL (`raw.Len = 3 + n`)
    #[cfg(target_vendor = "apple")]
    let len = {
        sa.sun_len = (3 + sock.len()) as u8;
        3 + sock.len()
    };
    // SAFETY: `sa` is a valid sockaddr_un and `len` does not exceed its size.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const sa).cast(),
            len as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(syscall("bind"));
    }
    // SAFETY: plain syscall on a socket we own.
    let rc = unsafe { libc::listen(fd.as_raw_fd(), listen_backlog()) };
    if rc < 0 {
        return Err(syscall("listen"));
    }
    Ok(std::os::unix::net::UnixListener::from(fd))
}

/// Go's `maxListenerBacklog` on darwin (`sock_bsd.go`): `sysctl kern.ipc.somaxconn`, else
/// `SOMAXCONN` when it fails or is 0; capped at 65535.
#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
fn listen_backlog() -> i32 {
    let mut n: u32 = 0;
    let mut len = std::mem::size_of::<u32>();
    // SAFETY: the name is NUL-terminated; `n` is valid for `len` bytes of output; no new value.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.ipc.somaxconn".as_ptr(),
            (&raw mut n).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 || n == 0 {
        return libc::SOMAXCONN;
    }
    n.min(0xffff) as i32
}

/// Go's `maxListenerBacklog`: the first number in `/proc/sys/net/core/somaxconn`, else 128.
#[cfg(target_os = "linux")]
fn listen_backlog() -> i32 {
    let Ok(text) = std::fs::read("/proc/sys/net/core/somaxconn") else {
        return libc::SOMAXCONN;
    };
    let digits: Vec<u8> = text
        .iter()
        .skip_while(|c| c.is_ascii_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .copied()
        .collect();
    match std::str::from_utf8(&digits)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    {
        Some(n) if n > 0 => i32::try_from(n).unwrap_or(i32::MAX),
        _ => libc::SOMAXCONN,
    }
}

/// `os.Chmod(path, mode)`.
fn chmod(path: &[u8], mode: u32) -> Result<(), GoErr> {
    use std::os::unix::fs::PermissionsExt;
    let Some(p) = gofs::nul_free(path) else {
        return Err(GoErr::path_errno("chmod", path, libc::EINVAL));
    };
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
        .map_err(|e| GoErr::path("chmod", path, &e))
}

/// `main()`.
pub fn main() -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        let _ = std::io::stdout().write_all(USAGE.as_bytes());
        std::process::exit(0);
    }
    if args.len() != 1 {
        let _ = std::io::stderr().write_all(USAGE.as_bytes());
        std::process::exit(2);
    }
    let sock = args[0].as_bytes().to_vec();
    let log = Logger::stderr();

    // 2. the cache directory: os.UserHomeDir on Linux reads $HOME only
    let cache = match env("XDG_CACHE_HOME") {
        Some(c) => c,
        None => match env("HOME") {
            Some(home) => gopath::join(&[&home, b".cache"]),
            None => fatal(&log, b"$HOME is not defined"),
        },
    };
    // 3. tiers, with the adopted strict budgets
    let mut tiers = vec![Tier {
        dir: gopath::join(&[&cache, b"ekacache", b"packs"]),
        budget: budget("EKACACHED_SIZE", 50 << 30),
    }];
    if let Some(cold) = env("EKACACHED_COLD") {
        tiers.push(Tier {
            dir: cold,
            budget: budget("EKACACHED_COLD_SIZE", 0),
        });
    }
    // 4. slots
    let limit = env("EKACACHED_SLOTS")
        .and_then(|v| ekacache_core::wire::parse_int64(&v))
        .filter(|&n| n > 0)
        .unwrap_or_else(num_cpu);
    let slots = Slots::new(limit);
    // 5. identities
    let store_dir = env("NIX_STORE_DIR").unwrap_or_else(|| b"/nix/store".to_vec());
    let idents = Identities::new(&store_dir);
    // 6. the store
    let mut cfg = StoreConfig::new(tiers);
    cfg.logger = log.clone();
    let store = match Store::open(cfg) {
        Ok(s) => s,
        Err(e) => fatal(&log, &e.text()),
    };
    for (i, t) in store.tiers().iter().enumerate() {
        let mut msg = format!("tier {i} ").into_bytes();
        msg.extend_from_slice(&t.dir);
        if t.budget > 0 {
            msg.extend_from_slice(format!(" budget {}", t.budget).as_bytes());
        } else {
            msg.extend_from_slice(b" budget unbounded");
        }
        log.log(&msg);
    }
    // 7-10. the socket
    if let Err(e) = gofs::mkdir_all(&gopath::dir(&sock), 0o755) {
        fatal(&log, &e.text());
    }
    let _ = gofs::remove(&sock);
    let listener = match listen_unix(&sock) {
        Ok(l) => l,
        Err(e) => fatal(&log, &e.text()),
    };
    if let Err(e) = chmod(&sock, 0o666) {
        fatal(&log, &e.text());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .build();
    let runtime = match runtime {
        Ok(rt) => rt,
        Err(e) => fatal(&log, e.to_string().as_bytes()),
    };
    let ctx = Arc::new(Ctx::new(
        store,
        idents,
        slots,
        Auth::from_system(),
        log.clone(),
    ));
    runtime.block_on(serve(listener, &sock, limit, &ctx));
    // the accept loop ended: seal and exit, without waiting for open connections (they keep
    // running on the runtime's threads until the process exits, as goroutines do)
    if let Err(e) = ctx.store.close() {
        fatal(&log, &e.text());
    }
    std::process::exit(0);
}

/// Steps 11 to 13, until the first SIGINT/SIGTERM or an accept error.
async fn serve(
    listener: std::os::unix::net::UnixListener,
    sock: &[u8],
    limit: i64,
    ctx: &Arc<Ctx>,
) {
    let log = &ctx.log;
    let listener = match tokio::net::UnixListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => fatal(log, e.to_string().as_bytes()),
    };
    // 11. SIGINT/SIGTERM: the first one closes the listener (and unlinks the socket, as Go's
    // UnixListener.Close does for a path it created); later ones are caught and ignored
    let (mut int, mut term) = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(i), Ok(t)) => (i, t),
        (Err(e), _) | (_, Err(e)) => fatal(log, e.to_string().as_bytes()),
    };
    let stop = Arc::new(Notify::new());
    let stopper = Arc::clone(&stop);
    let path = sock.to_vec();
    tokio::spawn(async move {
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
        }
        // Go's `UnixListener.close` skips a name starting with `@` on every OS, so on macOS,
        // where `@x` is an ordinary file, it stays until the next startup removes it
        if path.first() != Some(&b'@') {
            let _ = std::fs::remove_file(
                crate::gofs::nul_free(&path).unwrap_or(std::path::Path::new("")),
            );
        }
        stopper.notify_one();
        // keep the handlers installed: later signals are swallowed (Q-DAEMON-6)
        let _keep = (int, term);
        std::future::pending::<()>().await;
    });
    // 12.
    let mut msg = b"listening on ".to_vec();
    msg.extend_from_slice(sock);
    msg.extend_from_slice(format!(", {limit} slots, {}", ctx.store.stats()).as_bytes());
    log.log(&msg);
    // 13.
    server::accept_loop(&listener, ctx, &stop).await;
}
