//! `jig nix-store` (`behaviors/jig-nix-store.md`; `implementation/09-jig-nix-store.md`, phase
//! P8b): add text objects and ATerm derivations to the store and submit build outputs, over the
//! builder-rpc-v0 worker-protocol socket in `$NIX_REMOTE`, or offline with
//! `JIG_NIX_STORE_OFFLINE`.
//!
//! This file is `§Invocation`: the processing order (usage → open the store → dispatch → the
//! verb), `§add-text / add-drv`, `§fetchurls`, `§submit`, the exit statuses of `§Exit status
//! summary`, and every message of `§Errors and messages`.
//!
//! Nothing of the Q-NIX quirks is adopted: each is reproduced and marked `// Q-NIX-n` where it
//! happens. Q-NIX-1 (a dangling temporary in the C++) is C++-only undefined behaviour and is not
//! reproduced; the specified (P2718) behaviour is.

mod drv;
mod hash;
mod json;
mod offline;
mod rpc;
mod wire;

/// For the `nix_drv_json` fuzz target.
pub use drv::derivation_to_aterm;

use std::io::{Read, Write};

use jig_core::store_path::STORE_DIR;
use serde_json::Value;

use crate::Ctx;
use crate::process::ProcEnv;

// §Errors and messages, verbatim. Each goes to stderr followed by `\n`; `*_END` is the text after
// a substitution.

/// No arguments after `nix-store` (exit 2). `…` is U+2026, UTF-8 `E2 80 A6`.
pub const USAGE: &[u8] = b"usage: jig nix-store add-text|add-drv|fetchurls|submit \xE2\x80\xA6";
/// Online and `NIX_REMOTE` (verbatim, empty when unset) lacks the `unix://` prefix (exit 1).
pub const NIX_REMOTE_NOT_UNIX: &[u8] = b"jig nix-store: NIX_REMOTE=";
pub const NIX_REMOTE_NOT_UNIX_END: &[u8] = b" is not a unix:// socket (builder-rpc-v0 missing?)";
/// `socket()` or `connect()` failed; followed by the path (exit 1).
pub const CANNOT_CONNECT: &[u8] = b"jig nix-store: cannot connect to ";
/// The first flush failed, EOF, or the first word is not `WORKER_MAGIC_2` (exit 1).
pub const BAD_MAGIC: &[u8] = b"jig nix-store: bad daemon magic";
/// The daemon version (decimal; `0` when unreadable) is below 1.38 (exit 1).
pub const PROTOCOL_TOO_OLD: &[u8] = b"jig nix-store: daemon protocol ";
pub const PROTOCOL_TOO_OLD_END: &[u8] = b" too old";
/// `STDERR_ERROR` in a drain; followed by its `msg` (exit 1).
pub const DAEMON_ERROR: &[u8] = b"nix-daemon: ";
/// What follows `DAEMON_ERROR` when `msg` could not be read.
pub const DAEMON_ERROR_UNREADABLE: &[u8] = b"?";
/// An unknown stderr code in a drain; followed by `0x<lowercase hex>` (exit 1).
pub const UNEXPECTED_MESSAGE: &[u8] = b"jig nix-store: unexpected daemon message ";
/// `add-drv`: parse error, non-object top level, or a non-string `env` value (exit 1).
pub const DRV_DID_NOT_PARSE: &[u8] = b"jig nix-store: derivation JSON did not parse";
/// `fetchurls`: parse error or non-array top level (exit 1). No colon after `nix-store`.
pub const FETCHURLS_NOT_ARRAY: &[u8] = b"jig nix-store fetchurls: expected a JSON array";
/// Unknown verb or wrong argument count, after a successful open (exit 2).
pub const BAD_ARGUMENTS: &[u8] = b"jig nix-store: bad arguments";

/// `std::println(stderr, …)`: the parts and `\n`, in one write.
fn say(parts: &[&[u8]]) {
    let mut line = parts.concat();
    line.push(b'\n');
    let _ = std::io::stderr().write_all(&line);
}

/// `std::println(…)` to stdout.
fn print_line(text: &[u8]) {
    let mut line = text.to_vec();
    line.push(b'\n');
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&line);
    let _ = out.flush();
}

/// All of stdin as raw bytes; a read error ends it with what was read.
fn read_stdin() -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut buf);
    buf
}

/// Where objects go: the daemon, or nowhere with paths computed locally.
enum Store {
    Offline,
    Online(rpc::Daemon),
}

impl Store {
    /// `Opening`: offline when `JIG_NIX_STORE_OFFLINE` is non-empty, else connect and
    /// handshake.
    fn open(env: &ProcEnv) -> Option<Self> {
        // Q-NIX-3: any non-empty value, `0` and `false` included, means offline
        if env.non_empty("JIG_NIX_STORE_OFFLINE").is_some() {
            return Some(Self::Offline);
        }
        rpc::Daemon::connect(env).map(Self::Online)
    }

    /// Add a `text:sha256` object; its store path.
    fn add_text<R: AsRef<[u8]>>(
        &mut self,
        name: &[u8],
        refs: &[R],
        contents: &[u8],
    ) -> Option<Vec<u8>> {
        match self {
            Self::Offline => Some(offline::add_text(name, refs, contents)),
            Self::Online(d) => d.add_to_store(name, b"text:sha256", refs, contents),
        }
    }

    fn submit(&mut self, path: &[u8], output: &[u8]) -> bool {
        match self {
            Self::Offline => offline::submit(),
            Self::Online(d) => d.submit_output(path, output),
        }
    }
}

/// `RunNixStoreMode(args)`: `args` are the arguments after `nix-store`.
pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let Some((verb, rest)) = args.split_first() else {
        say(&[USAGE]);
        return 2;
    };
    // Q-NIX-2: the store is opened (online: connect and handshake) before the verb and the
    // argument count are checked
    let Some(mut store) = Store::open(&ctx.env) else {
        return 1;
    };
    // stdin is read only from here on, after the handshake
    match verb.as_slice() {
        b"add-text" | b"add-drv" if !rest.is_empty() => {
            add(&mut store, verb == b"add-drv", &rest[0], &rest[1..])
        }
        b"fetchurls" if rest.is_empty() => fetchurls(&mut store),
        b"submit" if rest.len() == 2 => i32::from(!store.submit(&rest[0], &rest[1])),
        _ => {
            // online, the connection is closed without sending any op
            say(&[BAD_ARGUMENTS]);
            2
        }
    }
}

/// `§add-text / add-drv`. The derivation's store name and references come from argv only
/// (Q-NIX-13).
fn add(store: &mut Store, drv: bool, name: &[u8], refs: &[Vec<u8>]) -> i32 {
    let mut contents = read_stdin();
    if drv {
        let Some(aterm) = drv::derivation_to_aterm(&contents) else {
            say(&[DRV_DID_NOT_PARSE]);
            return 1;
        };
        contents = aterm;
    }
    match store.add_text(name, refs, &contents) {
        Some(path) => {
            print_line(&path);
            0
        }
        None => 1,
    }
}

/// `§fetchurls`: one `builtin:fetchurl` `.drv` per element, in array order, then
/// `[{"drv":…,"out":…},…]`.
fn fetchurls(store: &mut Store) -> i32 {
    let input = read_stdin();
    let doc = json::parse(&input);
    let Some(Value::Array(files)) = doc.as_deref() else {
        say(&[FETCHURLS_NOT_ARRAY]);
        return 1;
    };
    let mut result: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for file in files {
        // Q-NIX-4: a non-object element or a non-string field aborts
        let name = json::value_str(file, "name");
        let algo = json::value_str(file, "algo");
        let hex = json::value_str(file, "hex");
        let out = hash::fixed_output_path(STORE_DIR, name, algo, hex);
        let aterm = drv::fetchurl_derivation(file, &out);
        let drv_name = [name, b".drv"].concat();
        // Q-NIX-17: a failure here exits with nothing on stdout, earlier .drvs stay added
        let Some(drv) = store.add_text::<&[u8]>(&drv_name, &[], &aterm) else {
            return 1;
        };
        result.push((drv, out));
    }
    // nlohmann `dump()`: keys in sorted order (`drv`, `out`), no spaces
    let mut text = b"[".to_vec();
    for (i, (drv, out)) in result.iter().enumerate() {
        if i > 0 {
            text.push(b',');
        }
        text.extend_from_slice(b"{\"drv\":");
        json::dump_string(&mut text, drv);
        text.extend_from_slice(b",\"out\":");
        json::dump_string(&mut text, out);
        text.push(b'}');
    }
    text.push(b']');
    print_line(&text);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TestNixStore` (`legacy/jig/src/jig_test.cc:635`).
    #[test]
    fn test_nix_store() {
        let hex = |d: &[u8]| String::from_utf8(jig_core::bytes::hex_encode(d)).unwrap();
        // FIPS 180-4 vectors
        assert_eq!(
            hex(&hash::sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&hash::sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&hash::sha256(&[b'a'; 1000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
        // `nix hash convert --hash-algo sha256 --to nix32 <hex of sha256("abc")>`
        assert_eq!(
            hash::nix32(&hash::sha256(b"abc")),
            b"1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"
        );
        // printf "" > e && nix store add --mode flat --hash-algo sha256 e
        assert_eq!(
            hash::fixed_output_path(
                b"/nix/store",
                b"e",
                b"sha256",
                b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            ),
            b"/nix/store/3hc40wwijh4im7g2i31nk75qappbjw9i-e"
        );
        // env sorted, inputSrcs sorted, output names sorted, quotes and newline escaped
        let aterm = drv::derivation_to_aterm(
            br#"{"name":"x","system":"x86_64-linux","builder":"/b","args":["-c","echo \"hi\"\n"],"env":{"out":"/o","b":"1","a":"2"},"inputSrcs":["/s2","/s1"],"inputDrvs":{"/d.drv":["out","dev"]},"outputs":{"out":{"hashAlgo":"r:sha256"}}}"#,
        );
        assert_eq!(
            aterm.as_deref(),
            Some(
                &br#"Derive([("out","","r:sha256","")],[("/d.drv",["dev","out"])],["/s1","/s2"],"x86_64-linux","/b",["-c","echo \"hi\"\n"],[("a","2"),("b","1"),("out","/o")])"#[..]
            )
        );
    }

    #[test]
    fn usage_bytes() {
        assert!(USAGE.ends_with("submit …".as_bytes()));
    }

    /// The messages match `§Errors and messages` in the behaviour doc.
    #[test]
    fn messages_match_the_doc() {
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../behaviors/jig-nix-store.md"
        ))
        .unwrap();
        let start = doc.find("## Errors and messages").unwrap();
        let end = start + doc[start..].find("## Test vectors").unwrap();
        let table = doc[start..end].replace("\\|", "|");
        let s = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap();
        let joined = |a: &[u8], b: &[u8]| format!("{}{}", s(a), s(b));
        for m in [
            s(USAGE),
            joined(NIX_REMOTE_NOT_UNIX, b"<value>") + &s(NIX_REMOTE_NOT_UNIX_END),
            joined(CANNOT_CONNECT, b"<path>"),
            s(BAD_MAGIC),
            joined(PROTOCOL_TOO_OLD, b"<n>") + &s(PROTOCOL_TOO_OLD_END),
            joined(DAEMON_ERROR, b"<msg>"),
            joined(DAEMON_ERROR, DAEMON_ERROR_UNREADABLE),
            joined(UNEXPECTED_MESSAGE, b"0x<hex>"),
            s(DRV_DID_NOT_PARSE),
            s(FETCHURLS_NOT_ARRAY),
            s(BAD_ARGUMENTS),
        ] {
            assert!(table.contains(&format!("`{m}`")), "{m}");
        }
    }
}
