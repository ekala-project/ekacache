//! Argument classification (`behaviors/jig-cc-mode.md §Argument classification`,
//! `cc_mode.cc:40-87`, `:446-643`): the suffix predicates, the scan, the after-scan rules and
//! `Classify`.

use std::cmp::Ordering;

use jig_core::bytes::{contains, find_byte_from};
use jig_core::fspath;

use super::Invocation;
use crate::process::ProcEnv;

/// `HasSuffix`: longer than the suffix and ending with it; a bare `.c` is no source.
fn has_suffix(arg: &[u8], suffixes: &[&[u8]]) -> bool {
    suffixes
        .iter()
        .any(|s| arg.len() > s.len() && arg.ends_with(s))
}

/// `IsAssembly`: `.S`, `.s`, `.sx`.
#[must_use]
pub fn is_assembly(path: &[u8]) -> bool {
    has_suffix(path, &[b".S", b".s", b".sx"])
}

/// `IsPlainAssembly`: `.s` is not preprocessed: `-MD` writes nothing and the source is the only
/// input.
#[must_use]
pub fn is_plain_assembly(path: &[u8]) -> bool {
    has_suffix(path, &[b".s"])
}

/// `IsSourceFile`.
#[must_use]
pub fn is_source_file(arg: &[u8]) -> bool {
    !arg.starts_with(b"-")
        && has_suffix(
            arg,
            &[
                b".c", b".cc", b".cpp", b".cxx", b".c++", b".C", b".S", b".s", b".sx", b".m",
            ],
        )
}

/// `IsObjectInput`.
#[must_use]
pub fn is_object_input(arg: &[u8]) -> bool {
    !arg.starts_with(b"-")
        && (has_suffix(arg, &[b".o", b".os", b".lo", b".a", b".so", b".obj"])
            || contains(arg, b".so."))
}

/// `HasLinkerSideOutput`: a glued linker option that writes or prints a second output
/// (Q-CC-9: the `-Xlinker -Map -Xlinker x` spelling escapes it).
#[must_use]
pub fn has_linker_side_output(arg: &[u8]) -> bool {
    const MARKERS: [&[u8]; 10] = [
        b"-Map",
        b"--print-map",
        b",-M,",
        b"--dependency-file",
        b"--out-implib",
        b"--trace",
        b",-t,",
        b"--verbose",
        b"--print-gc-sections",
        b"--stats",
    ];
    (arg.starts_with(b"-Wl,") || arg.starts_with(b"-Xlinker"))
        && (MARKERS.iter().any(|m| contains(arg, m))
            || arg.ends_with(b",-M")
            || arg.ends_with(b",-t"))
}

/// `IsNoOutputOption`: preprocess/dependency-only/query runs.
#[must_use]
pub fn is_no_output_option(arg: &[u8]) -> bool {
    matches!(
        arg,
        b"-M" | b"-MM" | b"-" | b"-v" | b"-V" | b"--version" | b"-###" | b"-fsyntax-only"
    ) || arg.starts_with(b"-print")
        || arg.starts_with(b"--print")
        || arg.starts_with(b"-dump")
}

/// `TakeDepfileOption` (`:452-467`): depfile options stay out of the key. Advances `i` past a
/// consumed value.
fn take_depfile_option<A: AsRef<[u8]>>(args: &[A], i: &mut usize, inv: &mut Invocation) -> bool {
    let arg = args[*i].as_ref();
    let has_next = *i + 1 < args.len();
    if arg == b"-MF" && has_next {
        *i += 1;
        inv.depfile = args[*i].as_ref().to_vec();
    } else if (arg == b"-MT" || arg == b"-MQ") && has_next {
        *i += 1;
        inv.depfile_target = args[*i].as_ref().to_vec();
    } else if arg.starts_with(b"-Wp,-MD,") || arg.starts_with(b"-Wp,-MMD,") {
        // kbuild's spelling; the prefix guarantees a ',' at or after offset 5
        let comma = find_byte_from(arg, b',', b"-Wp,-".len()).unwrap_or(arg.len() - 1);
        inv.depfile = arg[comma + 1..].to_vec();
    } else if arg != b"-MD" && arg != b"-MMD" && arg != b"-MP" {
        return false;
    }
    // Q-CC-6: -MT/-MQ/-MP/-MF alone also count
    inv.wants_depfile = true;
    true
}

/// `TakePchOption` (`:471-484`): `-include-pch <file>`, also cmake's
/// `-Xclang -include-pch -Xclang <file>`.
fn take_pch_option<A: AsRef<[u8]>>(args: &[A], i: &mut usize, inv: &mut Invocation) -> bool {
    if args[*i].as_ref() != b"-include-pch" {
        return false;
    }
    inv.key_args.push(args[*i].as_ref().to_vec());
    while *i + 1 < args.len() && args[*i + 1].as_ref() == b"-Xclang" {
        *i += 1;
        inv.key_args.push(args[*i].as_ref().to_vec());
    }
    if *i + 1 < args.len() {
        *i += 1;
        let pch = args[*i].as_ref().to_vec();
        inv.key_args.push(pch.clone());
        inv.pch.push(pch);
    }
    true
}

fn path_eq(a: &[u8], b: &[u8]) -> bool {
    fspath::compare(a, b) == Ordering::Equal
}

/// `Classify(inv, sources, objects, stop)` (`:492-518`).
fn classify(inv: &mut Invocation, sources: u32, objects: bool, stop: u8, env: &ProcEnv) {
    if inv.compile_only {
        inv.cacheable = inv.cacheable && sources == 1;
        inv.query = inv.query || (stop == b'E' && (sources == 0 || inv.source == b"/dev/null"));
        if stop == b'E' && (inv.output.is_empty() || path_eq(&inv.output, b"-")) {
            inv.to_stdout = true;
            // Q-CC-8. A set-but-empty $TMPDIR gives a relative `jig<pid>.i`
            let name = format!("jig{}.i", std::process::id());
            inv.output = fspath::join(env.get_or("TMPDIR", b"/tmp"), name.as_bytes());
        } else if inv.output.is_empty() {
            let mut out = fspath::stem(&inv.source).to_vec();
            out.extend_from_slice(if stop == b'S' { b".s" } else { b".o" });
            inv.output = out;
        }
    } else {
        inv.link_one = !objects && sources == 1;
        inv.link = objects && sources == 0 && !inv.inputs.is_empty();
        inv.cacheable = inv.cacheable && inv.links();
        if inv.output.is_empty() {
            inv.output = b"a.out".to_vec();
        }
        if inv.link {
            inv.source = fspath::filename(&inv.output).to_vec();
        }
    }
    if inv.wants_depfile && inv.depfile.is_empty() {
        inv.depfile = fspath::replace_extension(&inv.output, b".d");
    }
}

/// `ParseInvocation(args)` (`:586-643`). `env` supplies `$TMPDIR` for `-E` to stdout.
#[must_use]
pub fn parse_invocation<A: AsRef<[u8]>>(args: &[A], env: &ProcEnv) -> Invocation {
    let mut inv = Invocation {
        args: args.iter().map(|a| a.as_ref().to_vec()).collect(),
        ..Invocation::default()
    };
    let mut objects = false;
    let mut sources = 0u32;
    let mut stop = 0u8;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_ref();
        if take_depfile_option(args, &mut i, &mut inv) {
        } else if arg == b"-c" || arg == b"-S" || arg == b"-E" {
            inv.compile_only = true;
            stop = arg[1];
            inv.key_args.push(arg.to_vec());
        } else if arg == b"-o" && i + 1 < args.len() {
            i += 1;
            inv.output = args[i].as_ref().to_vec();
        } else if arg.starts_with(b"-o") && arg.len() > 2 {
            inv.output = arg[2..].to_vec();
        } else if is_no_output_option(arg) {
            inv.cacheable = false;
            inv.query = true;
        } else if is_source_file(arg) {
            inv.source = arg.to_vec();
            sources += 1;
        } else if take_pch_option(args, &mut i, &mut inv) {
        } else if is_object_input(arg) {
            objects = true;
            inv.inputs.push(arg.to_vec());
            inv.key_args.push(arg.to_vec());
        } else {
            objects = objects || arg == b"-shared" || arg == b"-r";
            // -Wl,@rsp hides inputs; -Map and friends write a second output; a PCH records the
            // absolute paths of the headers it read
            inv.cacheable = inv.cacheable
                && !arg.starts_with(b"@")
                && !contains(arg, b",@")
                && !has_linker_side_output(arg)
                && !arg.ends_with(b"-header");
            inv.key_args.push(arg.to_vec());
        }
        i += 1;
    }
    // /dev/null: a flag probe; the observed run would put temp files beside -o
    let ext = fspath::extension(&inv.output);
    inv.cacheable =
        inv.cacheable && ext != b".pch" && ext != b".gch" && !path_eq(&inv.output, b"/dev/null");
    // clang's frontend called directly: its own argument language
    if let Some(first) = args.first()
        && matches!(first.as_ref(), b"-cc1" | b"-cc1as")
    {
        inv.cacheable = false;
        inv.query = true;
    }
    classify(&mut inv, sources, objects, stop, env);
    // gold has no --dependency-file to learn a link's inputs from
    if inv.links() && args.iter().any(|a| a.as_ref() == b"-fuse-ld=gold") {
        inv.cacheable = false;
    }
    inv
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::env::Env;

    fn env() -> ProcEnv {
        ProcEnv::new(Env::from_pairs(&[("TMPDIR", "/t")]))
    }

    fn parse(args: &[&str]) -> Invocation {
        parse_invocation(args, &env())
    }

    fn v(args: &[&str]) -> Vec<Vec<u8>> {
        args.iter().map(|a| a.as_bytes().to_vec()).collect()
    }

    #[test]
    fn predicates() {
        assert!(!is_source_file(b".c") && is_source_file(b"a.c") && !is_source_file(b"-a.c"));
        for ok in [
            "x.cc", "x.cpp", "x.cxx", "x.c++", "x.C", "x.S", "x.s", "x.sx", "x.m",
        ] {
            assert!(is_source_file(ok.as_bytes()), "{ok}");
        }
        for no in ["x.mm", "x.i", "x.ii", "x.f90", "x.cppm", "x.H"] {
            assert!(!is_source_file(no.as_bytes()), "{no}");
        }
        assert!(is_assembly(b"a.S") && is_assembly(b"a.sx") && !is_assembly(b".s"));
        assert!(is_plain_assembly(b"a.s") && !is_plain_assembly(b"a.S"));
        assert!(is_object_input(b"libz.so.1.3") && is_object_input(b"a.lo"));
        assert!(!is_object_input(b"-lz.so.") && !is_object_input(b".o"));
        assert!(
            is_no_output_option(b"-print-file-name=libc.a") && is_no_output_option(b"-dumpversion")
        );
        assert!(is_no_output_option(b"--print-search-dirs") && !is_no_output_option(b"-MD"));
        for side in [
            "-Wl,-Map,x.map",
            "-Wl,--print-map",
            "-Wl,-M",
            "-Wl,-t",
            "-Wl,-M,-z",
            "-Xlinker--trace",
            "-Wl,--stats",
        ] {
            assert!(has_linker_side_output(side.as_bytes()), "{side}");
        }
        assert!(!has_linker_side_output(b"-Wl,-z,now") && !has_linker_side_output(b"-Map"));
    }

    // jig_test.cc TestParseJoinedOutput
    #[test]
    fn joined_output() {
        let inv = parse(&["-c", "foo.c", "-o/tmp/b/x.o"]);
        assert!(inv.cacheable && inv.output == b"/tmp/b/x.o" && inv.key_args == v(&["-c"]));
    }

    // jig_test.cc TestParseLink
    #[test]
    fn link() {
        let inv = parse(&["-o", "prog", "main.o", "libutil.a", "-lz", "-shared"]);
        assert!(inv.cacheable && inv.link && !inv.link_one);
        assert!(inv.output == b"prog" && inv.source == b"prog");
        assert_eq!(inv.inputs, v(&["main.o", "libutil.a"]));
        assert!(!parse(&["-o", "prog", "-Wl,@objs.rsp"]).cacheable);
        assert!(!parse(&["-shared", "-o", "x.so"]).cacheable);
        let inv = parse(&["-r", "-o", "m.o", "a.os", "-Wl,-Map,m.mapT"]);
        assert!(inv.link && !inv.cacheable);
        assert!(!parse(&["-mabi=lp64d", "-c", "cpuid.c", "-o", "/dev/null"]).cacheable);
        assert!(!parse(&["-c", "cpuid.c", "-o", "//dev//null"]).cacheable);
    }

    // jig_test.cc TestParseInvocation
    #[test]
    fn invocation() {
        let inv = parse(&[
            "-O2",
            "-c",
            "foo.c",
            "-o",
            "out/foo.o",
            "-MD",
            "-MF",
            "out/foo.d",
            "-MT",
            "x",
        ]);
        assert!(inv.cacheable && inv.compile_only && !inv.link_one);
        assert!(inv.source == b"foo.c" && inv.output == b"out/foo.o");
        assert!(inv.wants_depfile && inv.depfile == b"out/foo.d" && inv.depfile_target == b"x");
        assert_eq!(inv.key_args, v(&["-O2", "-c"]));

        let inv = parse(&["-c", "dir/foo.c"]);
        assert!(inv.cacheable && inv.output == b"foo.o" && !inv.wants_depfile);

        let inv = parse(&["-c", "foo.c", "-MD"]);
        assert!(inv.wants_depfile && inv.depfile == b"foo.d");

        let inv = parse(&["-Wp,-MMD,scripts/.fixdep.d", "-o", "fixdep", "fixdep.c"]);
        assert!(inv.cacheable && inv.link_one && inv.wants_depfile);
        assert!(inv.depfile == b"scripts/.fixdep.d" && inv.output == b"fixdep");
        let inv = parse(&["-Wp,-MD,a,b.d", "-c", "x.c"]);
        assert_eq!(inv.depfile, b"a,b.d");

        let inv = parse(&["-O2", "conftest.c"]);
        assert!(inv.cacheable && inv.link_one && inv.output == b"a.out");

        assert!(!parse(&["-o", "conftest", "conftest.c", "conftstm.o"]).cacheable);
        assert!(!parse(&["-shared", "-o", "lib.so", "a.c"]).cacheable);
        for flag in ["-M", "--version", "-print-search-dirs", "-fsyntax-only"] {
            let inv = parse(&[flag, "conftest.c"]);
            assert!(!inv.cacheable && inv.query, "{flag}");
        }
        assert!(!parse(&["-c", "a.c", "b.c"]).cacheable);
        assert!(!parse(&["-c", "-x", "c", "-"]).cacheable);
        // -MF as the last argument is no depfile option
        let inv = parse(&["-c", "a.c", "-MF"]);
        assert!(!inv.wants_depfile && inv.key_args == v(&["-c", "-MF"]));
    }

    // jig_test.cc TestParsePch
    #[test]
    fn pch() {
        assert!(!parse(&["-x", "c++-header", "pch.hxx", "-o", "pch.hxx.pch", "-c"]).cacheable);
        assert!(
            !parse(&[
                "-Xclang",
                "-emit-pch",
                "-c",
                "cmake_pch.hxx.cxx",
                "-o",
                "cmake_pch.hxx.pch"
            ])
            .cacheable
        );
        let inv = parse(&["-include-pch", "x.pch", "-c", "a.cc", "-o", "a.o"]);
        assert!(inv.cacheable && inv.pch == v(&["x.pch"]));
        assert_eq!(inv.key_args, v(&["-include-pch", "x.pch", "-c"]));
        let inv = parse(&[
            "-Xclang",
            "-include-pch",
            "-Xclang",
            "/b/x.pch",
            "-Xclang",
            "-include",
            "-Xclang",
            "/b/x.hxx",
            "-c",
            "a.cc",
        ]);
        assert!(inv.cacheable && inv.pch == v(&["/b/x.pch"]) && inv.source == b"a.cc");
    }

    // jig_test.cc TestParsePreprocess
    #[test]
    fn preprocess() {
        let pid = std::process::id();
        let inv = parse(&["-std=gnu23", "-E", "conftest.c"]);
        assert!(inv.cacheable && inv.compile_only && inv.to_stdout);
        assert_eq!(inv.key_args, v(&["-std=gnu23", "-E"]));
        assert_eq!(inv.output, format!("/t/jig{pid}.i").as_bytes());
        let inv = parse(&["-E", "conftest.c", "-o", "-"]);
        assert!(inv.cacheable && inv.to_stdout);
        let inv = parse(&["-E", "-o", "x.i", "x.c"]);
        assert!(inv.cacheable && !inv.to_stdout && inv.output == b"x.i");
        let inv = parse(&["-S", "x.c"]);
        assert!(inv.cacheable && !inv.to_stdout && inv.output == b"x.s");
        assert_eq!(inv.key_args, v(&["-S"]));
        assert!(!parse(&["-E", "-"]).cacheable);
        let inv = parse(&["-E", "-x", "c", "/dev/null"]);
        assert!(inv.query);
        // the last of -c/-S/-E decides
        assert_eq!(parse(&["-E", "-c", "x.c"]).output, b"x.o");
        // $TMPDIR unset and set-but-empty
        let unset = parse_invocation(&["-E", "x.c"], &ProcEnv::new(Env::default()));
        assert_eq!(unset.output, format!("/tmp/jig{pid}.i").as_bytes());
        let empty = parse_invocation(
            &["-E", "x.c"],
            &ProcEnv::new(Env::from_pairs(&[("TMPDIR", "")])),
        );
        assert_eq!(empty.output, format!("jig{pid}.i").as_bytes());
        // -E -MD to stdout: the depfile beside the temp file (Q-CC-8)
        let inv = parse(&["-E", "-MD", "x.c"]);
        assert_eq!(inv.depfile, format!("/t/jig{pid}.d").as_bytes());
    }

    // jig_test.cc TestParseGuards
    #[test]
    fn guards() {
        let inv = parse(&["-cc1", "-emit-obj", "-o", "a.o", "a.c"]);
        assert!(!inv.cacheable && inv.query);
        assert!(!parse(&["-fuse-ld=gold", "a.o", "-o", "a"]).cacheable);
        assert!(parse(&["-fuse-ld=bfd", "a.o", "-o", "a"]).cacheable);
        assert!(!parse(&["-c", "a.c", "@missing.rsp"]).cacheable);
        assert!(!parse(&["-x", "c-header", "-c", "a.c"]).cacheable);
        // -Xlinker -Map -Xlinker x escapes the side-output check (Q-CC-9)
        assert!(parse(&["a.o", "-Xlinker", "-Map", "-Xlinker", "x.map", "-o", "a"]).cacheable);
        // sources mixed with objects
        let inv = parse(&["a.c", "b.o", "-o", "a"]);
        assert!(!inv.cacheable && !inv.link && !inv.link_one);
        // no inputs at all
        assert!(!parse(&["-o", "a"]).cacheable);
    }
}
