//! Lookup and replay (`behaviors/jig-cc-mode.md §Lookup and replay`, `cc_mode.cc:192-283`).

use std::os::unix::fs::PermissionsExt;

use jig_core::bytes::{find, find_byte_from, os_path, parse_uint};
use jig_core::keys::{RequestKey, ResultKey, slot};
use jig_core::manifest;
use jig_core::store_path::Store;

use super::uncacheable::mentions_incbin;
use super::{Invocation, is_assembly, stderr, stdout_flushed};
use crate::client::Client;
use crate::sys;
use crate::util::{read_file, write_file_errno};

/// Everything a hit has to reproduce (`CachedResult`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CachedResult {
    pub status: i32,
    pub object: Option<Vec<u8>>,
    pub depfile: Option<Vec<u8>>,
    pub stderr_text: Vec<u8>,
}

/// Why there is nothing to replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Miss {
    /// `new-key`, `inputs-changed:<path>`, `appeared:<path>`, `object-gone` or `incbin`.
    pub why: Vec<u8>,
    /// ADOPTED Q-CC-12 (rule C on a hit): the compile that follows is not stored.
    pub store: bool,
}

impl Miss {
    fn because(why: &[u8]) -> Self {
        Self {
            why: why.to_vec(),
            store: true,
        }
    }
}

/// `FindResult`, keeping the manifest text for the rule C check.
fn find_result(
    client: &mut Client,
    store: &Store,
    k1: &RequestKey,
) -> Result<(ResultKey, Vec<u8>), Vec<u8>> {
    let Some(text) = client.get(&slot::manifest(k1)) else {
        return Err(b"new-key".to_vec());
    };
    let k2 = manifest::validate(store, k1, &text, &mut |paths: &[&[u8]]| {
        client.identities(paths)
    })?;
    Ok((k2, text))
}

/// ADOPTED Q-CC-12 (rule C on a validated hit): an input of the stored manifest whose bytes
/// mention `.incbin`. Guards against entries the C++ jig wrote.
fn manifest_mentions_incbin(store: &Store, text: &[u8]) -> bool {
    manifest::parse(store, text).iter().any(|e| {
        e.tab.is_some()
            && !e.path.is_empty()
            && read_file(&e.path).is_some_and(|bytes| mentions_incbin(&bytes))
    })
}

/// `Lookup(cache, k1, inv)` (`:201-235`): the manifest, k2, then `r/ o/ d/ e/` in one pipelined
/// exchange.
pub fn lookup(
    client: &mut Client,
    store: &Store,
    k1: &RequestKey,
    inv: &Invocation,
) -> Result<CachedResult, Miss> {
    let (k2, text) = find_result(client, store, k1).map_err(|why| Miss { why, store: true })?;
    if !inv.link && is_assembly(&inv.source) && manifest_mentions_incbin(store, &text) {
        return Err(Miss {
            why: b"incbin".to_vec(),
            store: false,
        });
    }
    let keys = [
        slot::exit_status(&k2),
        slot::object(&k2),
        slot::depfile(&k2),
        slot::stderr(&k2),
    ];
    let mut got = client.get_many(&keys).into_iter();
    let mut next = || got.next().flatten();
    let (status, object, depfile, stderr_text) = (next(), next(), next(), next());
    let mut result = CachedResult::default();
    // only cached failures have an exit status. Q-CC-1: unparsable is 1; the cast truncates as
    // legacy's static_cast<int>
    if !inv.links()
        && let Some(status) = status
    {
        #[allow(clippy::cast_possible_truncation)]
        let parsed = parse_uint(&status).map_or(1, |v| v as i32);
        result.status = parsed;
    }
    if result.status == 0 {
        // depfile options are not in the key: an entry stored without -MD lacks one
        let Some(object) = object else {
            return Err(Miss::because(b"object-gone"));
        };
        if inv.wants_depfile && depfile.is_none() {
            return Err(Miss::because(b"object-gone"));
        }
        result.object = Some(store.unmask_out(&object).into_owned());
        if inv.wants_depfile {
            result.depfile = depfile.map(|d| store.unmask_out(&d).into_owned());
        }
    }
    result.stderr_text = store
        .unmask_out(&stderr_text.unwrap_or_default())
        .into_owned();
    Ok(result)
}

/// `ForwardStdout(inv, text)` (`:238-248`): `-E` to stdout ran (or replays) into a temp file.
pub fn forward_stdout(inv: &Invocation, text: Option<&[u8]>) {
    if !inv.to_stdout {
        return;
    }
    sys::remove(&inv.output);
    if let Some(text) = text {
        stdout_flushed(text);
    }
}

/// `RetargetDepfile(text, inv)` (`:252-259`): the first rule's target becomes this run's
/// (`-MT`/`-MQ`, else the output): the entry may come from a run whose names differed only in a
/// masked random part.
#[must_use]
pub fn retarget_depfile(text: &[u8], inv: &Invocation) -> Vec<u8> {
    let target = if inv.depfile_target.is_empty() {
        &inv.output
    } else {
        &inv.depfile_target
    };
    match find(text, b": ") {
        Some(colon) if find_byte_from(text, b'\n', 0).unwrap_or(text.len()) > colon => {
            let mut out = target.clone();
            out.extend_from_slice(&text[colon..]);
            out
        }
        _ => text.to_vec(),
    }
}

/// `jig: cannot write <path>: <strerror>`.
fn cannot_write(path: &[u8], errno: i32) {
    let mut msg = b"jig: cannot write ".to_vec();
    msg.extend_from_slice(path);
    msg.extend_from_slice(b": ");
    msg.extend_from_slice(&sys::strerror(errno));
    msg.push(b'\n');
    stderr(&msg);
}

/// `Replay(result, inv)` (`:261-283`): output, depfile, stderr; the cached status.
pub fn replay(result: &CachedResult, inv: &Invocation, store: &Store) -> i32 {
    if inv.to_stdout {
        forward_stdout(inv, result.object.as_deref());
    } else if let Some(object) = &result.object {
        if let Err(errno) = write_file_errno(&inv.output, object) {
            cannot_write(&inv.output, errno);
            return 1;
        }
        if inv.links() {
            // Q-CC-11: `0666 & ~umask` plus +x, ignoring errors
            if let Ok(meta) = std::fs::metadata(os_path(&inv.output)) {
                let mode = (meta.permissions().mode() & 0o7777) | 0o111;
                let _ = std::fs::set_permissions(
                    os_path(&inv.output),
                    std::fs::Permissions::from_mode(mode),
                );
            }
        }
    }
    if let Some(depfile) = &result.depfile {
        // ADOPTED Q-KEY-1 / Q-CC-7: resolve_all is the identity in path mode
        let text = retarget_depfile(&store.resolve_all(depfile), inv);
        if let Err(errno) = write_file_errno(&inv.depfile, &text) {
            cannot_write(&inv.depfile, errno);
            return 1;
        }
    }
    stderr(&result.stderr_text);
    result.status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retarget() {
        let inv = Invocation {
            output: b"out/a.o".to_vec(),
            ..Invocation::default()
        };
        assert_eq!(
            retarget_depfile(b"cmTC_1.o: a.c \\\n  b.h\n", &inv),
            b"out/a.o: a.c \\\n  b.h\n"
        );
        // the first ": " lies after the first newline: unchanged
        assert_eq!(retarget_depfile(b"x\ny: z\n", &inv), b"x\ny: z\n");
        assert_eq!(retarget_depfile(b"no colon", &inv), b"no colon");
        assert_eq!(retarget_depfile(b"a.o: b", &inv), b"out/a.o: b");
        let mt = Invocation {
            depfile_target: b"$(OBJ)".to_vec(),
            ..inv
        };
        assert_eq!(retarget_depfile(b"a.o: b\nb:\n", &mt), b"$(OBJ): b\nb:\n");
        // C:\ paths: the first ": " counts, not the first ':'
        assert_eq!(retarget_depfile(b"c:x.o: y", &mt), b"$(OBJ): y");
    }
}
