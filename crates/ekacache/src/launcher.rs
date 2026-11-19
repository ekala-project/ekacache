//! Launcher mode: `jig /path/to/<program> <args...>` (`behaviors/jig-dispatch.md §Launcher mode`,
//! `launcher.cc`).

use ekacache_core::fspath;
use ekacache_core::manifest::exists;

use crate::{Ctx, cc, process, rustc};

/// What `ClassifyLaunch` makes of a program's basename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cc,
    Rustc,
    Go,
    Other,
}

/// `StripVersion`: a trailing `-<digits and dots>` is dropped (`gcc-15` → `gcc`); a `-` that is
/// the last byte is kept.
#[must_use]
pub fn strip_version(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&c| c == b'-') {
        Some(dash) if dash + 1 < name.len() => {
            if name[dash + 1..]
                .iter()
                .all(|&c| c.is_ascii_digit() || c == b'.')
            {
                &name[..dash]
            } else {
                name
            }
        }
        _ => name,
    }
}

/// `ClassifyLaunch(basename)`.
#[must_use]
pub fn classify(basename: &[u8]) -> Kind {
    const CC: [&[u8]; 6] = [b"cc", b"c++", b"gcc", b"g++", b"clang", b"clang++"];
    let name = strip_version(basename);
    for driver in CC {
        // bare, or the last word of a target-prefixed name (Q-DISPATCH-8: any `-cc` suffix)
        if name == driver
            || (name.len() > driver.len()
                && name.ends_with(driver)
                && name[name.len() - driver.len() - 1] == b'-')
        {
            return Kind::Cc;
        }
    }
    match name {
        b"rustc" => Kind::Rustc,
        b"go" => Kind::Go,
        _ => Kind::Other,
    }
}

/// `RunLauncher(program, args, socket, self)`.
pub fn run(program: &[u8], args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let basename = fspath::filename(program);
    match classify(basename) {
        Kind::Cc => cc::run(basename, args, Some(program), ctx),
        Kind::Rustc => {
            let mut wrapped = Vec::with_capacity(args.len() + 1);
            wrapped.push(program.to_vec());
            wrapped.extend_from_slice(args);
            rustc::run(&wrapped, ctx)
        }
        Kind::Go => {
            // Q-DISPATCH-7 kept: an existence check, no connect
            if ctx.env.non_empty("GOCACHEPROG").is_none() && exists(&ctx.socket) {
                let mut prog = ctx.self_path();
                prog.extend_from_slice(b" gocacheprog");
                ctx.env.set("GOCACHEPROG", prog);
            }
            process::exec(program, args, &ctx.env)
        }
        Kind::Other => process::exec(program, args, &ctx.env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_version_examples() {
        assert_eq!(strip_version(b"gcc-15"), b"gcc");
        assert_eq!(strip_version(b"clang++-21.1"), b"clang++");
        assert_eq!(strip_version(b"gcc-"), b"gcc-");
        assert_eq!(strip_version(b"clang-cpp"), b"clang-cpp");
        assert_eq!(strip_version(b"-1"), b"");
        assert_eq!(strip_version(b"gcc"), b"gcc");
    }

    #[test]
    fn classify_table() {
        // behaviors/jig-dispatch.md §ClassifyLaunch
        for name in [
            "gcc",
            "g++",
            "cc",
            "c++",
            "clang",
            "clang++",
            "x86_64-unknown-linux-gnu-gcc",
            "aarch64-linux-gnu-g++",
            "gcc-15",
            "clang++-21.1",
            "x86_64-unknown-linux-gnu-gcc-14.2",
            "mpi-cc",
            "-cc",
        ] {
            assert_eq!(classify(name.as_bytes()), Kind::Cc, "{name}");
        }
        for name in [
            "mpicc",
            "distcc",
            "tcc",
            "gcc-ar",
            "clang-cpp",
            "gfortran",
            "gcc-",
            "go1.22",
            "gofmt",
            "",
        ] {
            assert_eq!(classify(name.as_bytes()), Kind::Other, "{name}");
        }
        assert_eq!(classify(b"rustc"), Kind::Rustc);
        assert_eq!(classify(b"rustc-1.80"), Kind::Rustc);
        assert_eq!(classify(b"go"), Kind::Go);
    }
}
