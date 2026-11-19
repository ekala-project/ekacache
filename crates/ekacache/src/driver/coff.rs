//! The COFF policy (`behaviors/jig-driver-policy.md` A12): PE is position independent by
//! construction and clang rejects the PIC flags; MSVC's STL has no C++11 mode; Windows library
//! names are case-insensitive but the build host compares bytes.

use super::args::UserArgs;
use super::{DriverConf, exists_or_abort, flag_value};

fn is_pic_arg(arg: &[u8]) -> bool {
    matches!(
        arg,
        b"-fPIC" | b"-fpic" | b"-fPIE" | b"-fpie" | b"-fno-PIC" | b"-fno-pic" | b"-pie"
    )
}

/// `CoffArg`: drop the PIC flags, respell `-std=…++98/03/0x/11` as `…++14`.
pub fn coff_arg(arg: &[u8]) -> Option<Vec<u8>> {
    if is_pic_arg(arg) {
        return None;
    }
    let std = arg.starts_with(b"-std=c") || arg.starts_with(b"-std=gnu");
    for old in [&b"++98"[..], b"++03", b"++0x", b"++11"] {
        if std && arg.ends_with(old) {
            let mut out = arg[..arg.len() - 2].to_vec();
            out.extend_from_slice(b"14");
            return Some(out);
        }
    }
    Some(arg.to_vec())
}

/// `CoffLibs`: a `-l<name>` (no `/`) that no `-L` dir has as `lib<name>.dll.a`, `lib<name>.a`
/// or `<name>` is lower-cased, ASCII only (Q-DRV-11).
pub fn coff_libs(conf: &DriverConf, user: &mut UserArgs) {
    let mut dirs: Vec<Vec<u8>> = Vec::new();
    for args in [&user.args, &conf.package.ldflags] {
        for i in 0..args.len() {
            if let Some(dir) = flag_value(args, i, b"-L") {
                dirs.push(dir.to_vec());
            }
        }
    }
    let present = |name: &[u8]| {
        dirs.iter().any(|dir| {
            let with = |pre: &[u8], post: &[u8]| {
                let mut p = dir.clone();
                p.extend_from_slice(pre);
                p.extend_from_slice(name);
                p.extend_from_slice(post);
                p
            };
            exists_or_abort(&with(b"/lib", b".dll.a"))
                || exists_or_abort(&with(b"/lib", b".a"))
                || exists_or_abort(&with(b"/", b""))
        })
    };
    for arg in &mut user.args {
        if !arg.starts_with(b"-l") || arg.contains(&b'/') || present(&arg[2..]) {
            continue;
        }
        arg[2..].make_ascii_lowercase();
    }
}
