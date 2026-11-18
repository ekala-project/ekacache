//! The cc request key k1 (`behaviors/jig-cc-mode.md §Request key (k1)`,
//! `behaviors/jig-keys.md §k1 field sequences`, `cc_mode.cc:98-128`).

use jig_core::bytes::contains;
use jig_core::hasher::Hasher;
use jig_core::keys::{RequestKey, Tool, key_environment};
use jig_core::store_path::Store;

use super::Invocation;
use crate::process::ProcEnv;

/// `fs::current_path()`. Legacy throws `filesystem_error` when `getcwd` fails and dies by
/// `std::terminate`; the port aborts without libstdc++'s text (as for Q-DRV-10).
pub fn current_dir() -> Vec<u8> {
    match std::env::current_dir() {
        Ok(cwd) => cwd.into_os_string().into_encoded_bytes(),
        Err(_) => std::process::abort(),
    }
}

/// `ComputeRequestKey(compiler, inv, primary)`: `cc=`, `cwd=`, `src=` (not for `link`), `mode=`,
/// `LIBRARY_PATH=` (links), `env=`, the key args, the primary. `cwd` is the current directory.
#[must_use]
pub fn request_key(
    store: &Store,
    compiler: &[u8],
    inv: &Invocation,
    primary: &[u8],
    env: &ProcEnv,
    cwd: &[u8],
) -> RequestKey {
    let links = inv.links();
    let field = |name: &[u8], value: &[u8]| [name, value].concat();
    let mut h = Hasher::new();
    h.field(&field(b"cc=", &store.tool_id(compiler)));
    // relative -I/-include and __FILE__ depend on it
    h.field(&field(b"cwd=", &store.key(cwd)));
    // identical bytes at another path are another __FILE__, DW_AT_name and depfile prerequisite
    if !inv.link {
        h.field(&field(b"src=", &store.key(&store.mask_out(&inv.source))));
    }
    h.field(if inv.link {
        b"mode=link"
    } else if inv.link_one {
        b"mode=link-one"
    } else {
        b"mode=compile"
    });
    if links {
        let library_path = env.get_or("LIBRARY_PATH", b"");
        h.field(&field(
            b"LIBRARY_PATH=",
            &store.mask_for_replay(library_path),
        ));
    }
    let environment = key_environment(env.env().raw_entries(), links);
    h.field(&field(
        b"env=",
        &store.mask_for_replay(&store.mask_out(&environment)),
    ));
    // never Key()ed: dependency paths a link embeds verbatim (PT_INTERP, RUNPATH)
    for arg in &inv.key_args {
        let embedded = links && (contains(arg, b"dynamic-linker") || contains(arg, b"rpath"));
        let masked = store.mask_out(arg);
        if embedded {
            h.field(&masked);
        } else {
            h.field(&store.key(&masked));
        }
    }
    h.field(&store.mask_out(primary));
    RequestKey::new(Tool::Cc, &h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::env::Env;

    // jig_test.cc TestKeyEnvironment, through the jig-core function k1 uses
    #[test]
    fn key_environment_vector() {
        let env = [
            "PATH=/bin",
            "NIX_HARDENING_ENABLE=bindnow relro",
            "NIX_BUILD_TOP=/build",
            "CPATH=/inc",
            "NIX_LDFLAGS=-rpath /nix/store/x/lib",
            "NIX_LOG_FD=2",
            "NIX_ATTRS_SH_FILE=/build/.attrs.sh",
        ];
        assert_eq!(key_environment(env, false), b"CPATH=/inc\n");
        assert_eq!(
            key_environment(env, true),
            b"CPATH=/inc\nNIX_HARDENING_ENABLE=bindnow relro\nNIX_LDFLAGS=-rpath /nix/store/x/lib\n"
        );
    }

    /// The field sequence, spelt out with the hasher.
    #[test]
    fn fields() {
        let store = Store::from_env(&Env::default());
        let env = ProcEnv::new(Env::from_pairs(&[
            ("CPATH", "/inc"),
            ("NIX_LDFLAGS", "-L/x"),
            ("LIBRARY_PATH", "/lib"),
        ]));
        let inv = Invocation {
            source: b"a.c".to_vec(),
            key_args: vec![b"-O2".to_vec(), b"-Wl,-rpath,/r".to_vec()],
            compile_only: true,
            ..Invocation::default()
        };
        let mut h = Hasher::new();
        h.field(&[b"cc=".as_slice(), &store.tool_id(b"/cc/bin/clang")].concat())
            .field(b"cwd=/w")
            .field(b"src=a.c")
            .field(b"mode=compile")
            .field(b"env=CPATH=/inc\n")
            .field(b"-O2")
            .field(&store.key(b"-Wl,-rpath,/r"))
            .field(b"int x;");
        let want = RequestKey::new(Tool::Cc, &h.finish());
        let got = request_key(&store, b"/cc/bin/clang", &inv, b"int x;", &env, b"/w");
        assert_eq!(got, want);

        let link = Invocation {
            link: true,
            source: b"prog".to_vec(),
            key_args: vec![b"a.o".to_vec(), b"-Wl,-rpath,/r".to_vec()],
            ..Invocation::default()
        };
        let mut h = Hasher::new();
        h.field(&[b"cc=".as_slice(), &store.tool_id(b"cc")].concat())
            .field(b"cwd=/w")
            .field(b"mode=link")
            .field(b"LIBRARY_PATH=/lib")
            .field(b"env=CPATH=/inc\nNIX_LDFLAGS=-L/x\n")
            .field(&store.key(b"a.o"))
            .field(b"-Wl,-rpath,/r")
            .field(b"a.o=C:x\n");
        let want = RequestKey::new(Tool::Cc, &h.finish());
        assert_eq!(
            request_key(&store, b"cc", &link, b"a.o=C:x\n", &env, b"/w"),
            want
        );
        let one = Invocation {
            link_one: true,
            ..link.clone()
        };
        assert_ne!(request_key(&store, b"cc", &one, b"", &env, b"/w"), want);
    }
}
