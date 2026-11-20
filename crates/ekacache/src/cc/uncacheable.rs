//! The adopted uncacheable rules A–D (`behaviors/ec-cc-mode.md §Additional uncacheable cases`,
//! Q-CC-12..16, signed off 2026-11-17). The C++ jig caches all of these.
//!
//! A, B and D are matched against the user arguments after response-file expansion (not the
//! injected flags) for every invocation kind; D also against the primary source. C (`.incbin`)
//! is checked before lookup (`super::run`), on a validated hit (`super::lookup`) and before
//! storing (`super::store`), for assembler primaries only.

use ekacache_core::bytes::{contains, split};

/// `arg` is `flag` or `flag=…`.
fn exact_or_value(arg: &[u8], flag: &[u8]) -> bool {
    arg.strip_prefix(flag)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"="))
}

/// Rule A (Q-CC-14, Q-CC-15): a flag that makes the compiler write a second output a hit does
/// not restore, or a C++ modules flag.
#[must_use]
pub fn side_output(arg: &[u8]) -> bool {
    const EXACT: [&[u8]; 8] = [
        b"--coverage",
        b"-ftest-coverage",
        b"-fstack-usage",
        b"--precompile",
        b"-fdiagnostics-format=sarif-file",
        b"-fmodules-ts",
        b"-fmodules",
        b"-fmodule-output",
    ];
    const WITH_VALUE: [&[u8]; 5] = [
        b"-gsplit-dwarf",
        b"-save-temps",
        b"-ftime-trace",
        b"-fcallgraph-info",
        b"-fsave-optimization-record",
    ];
    const PREFIX: [&[u8]; 9] = [
        b"-foptimization-record-file=",
        b"-fdump-",
        b"-MJ",
        b"-fdiagnostics-add-output=",
        b"-fmodule-output=",
        b"-fmodule-file=",
        b"-fmodule-mapper=",
        b"-fprebuilt-module-path=",
        b"-fdeps-",
    ];
    if EXACT.contains(&arg) || WITH_VALUE.iter().any(|f| exact_or_value(arg, f)) {
        return true;
    }
    if arg.starts_with(b"-fopt-info") {
        return contains(arg, b"=");
    }
    if PREFIX.iter().any(|p| arg.starts_with(p)) {
        return true;
    }
    // an assembler listing: -Wa,-adhln=x.lst, -Wa,-a=x
    arg.starts_with(b"-Wa,")
        && split(arg, b',', true)
            .iter()
            .any(|e| e.starts_with(b"-a") && contains(e, b"="))
}

/// Rule B (Q-CC-13): a flag naming an input the depfile does not list. `next` is the following
/// argument, for `-Xclang -load` / `-Xclang -plugin`.
#[must_use]
pub fn hidden_input(arg: &[u8], next: Option<&[u8]>) -> bool {
    const PREFIX: [&[u8]; 6] = [
        b"-fprofile-instr-use=",
        b"-fprofile-sample-use=",
        b"-fplugin=",
        b"-fpass-plugin=",
        b"-specs=",
        b"--specs=",
    ];
    exact_or_value(arg, b"-fprofile-use")
        || exact_or_value(arg, b"-fauto-profile")
        || PREFIX.iter().any(|p| arg.starts_with(p))
        || (arg == b"-Xclang" && matches!(next, Some(b"-load" | b"-plugin")))
}

/// `[0-9]+` (ASCII), non-empty.
fn digits(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(u8::is_ascii_digit)
}

/// Rule D (Q-CC-16): a whole path component another toolchain's temp files live under:
/// `go-build[0-9]+` (cgo's `$WORK`), `ghc[0-9]+_[0-9]+`, `tmpxft_…` (nvcc), `camlasm…`
/// (ocamlopt).
#[must_use]
pub fn foreign_temp_component(component: &[u8]) -> bool {
    if let Some(rest) = component.strip_prefix(b"go-build") {
        return digits(rest);
    }
    if let Some(rest) = component.strip_prefix(b"ghc")
        && let Some(us) = rest.iter().position(|&c| c == b'_')
    {
        return digits(&rest[..us]) && digits(&rest[us + 1..]);
    }
    component.starts_with(b"tmpxft_") || component.starts_with(b"camlasm")
}

fn foreign_temp(path: &[u8]) -> bool {
    split(path, b'/', true)
        .iter()
        .any(|c| foreign_temp_component(c))
}

/// Rules A, B and D over the user arguments and the primary source.
#[must_use]
pub fn adopted_rules_refuse<A: AsRef<[u8]>>(args: &[A], primary_source: &[u8]) -> bool {
    let refuse = |i: usize| {
        let arg = args[i].as_ref();
        side_output(arg)
            || hidden_input(arg, args.get(i + 1).map(AsRef::as_ref))
            || foreign_temp(arg)
    };
    (0..args.len()).any(refuse) || foreign_temp(primary_source)
}

/// Rule C (Q-CC-12): `.incbin` makes the assembler read a file no depfile lists. A plain byte
/// search: a match in a comment also counts.
#[must_use]
pub fn mentions_incbin(text: &[u8]) -> bool {
    contains(text, b".incbin")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refuses(args: &[&str]) -> bool {
        adopted_rules_refuse(args, b"")
    }

    #[test]
    fn rule_a_side_outputs() {
        for arg in [
            "--coverage",
            "-ftest-coverage",
            "-gsplit-dwarf",
            "-gsplit-dwarf=single",
            "-save-temps",
            "-save-temps=obj",
            "-ftime-trace",
            "-ftime-trace=t.json",
            "-fstack-usage",
            "-fcallgraph-info",
            "-fcallgraph-info=su,da",
            "-fdump-tree-all",
            "-fdump-rtl-expand",
            "-fopt-info-vec=v.txt",
            "-fopt-info=x",
            "-fsave-optimization-record",
            "-fsave-optimization-record=yaml",
            "-foptimization-record-file=x.yaml",
            "-MJ",
            "-MJx.json",
            "-fdiagnostics-format=sarif-file",
            "-fdiagnostics-add-output=sarif:file=x.sarif",
            "-Wa,-adhln=x.lst",
            "-Wa,--noexecstack,-a=x.lst",
            "-fmodules-ts",
            "-fmodules",
            "-fmodule-output",
            "-fmodule-output=x.pcm",
            "-fmodule-file=a=a.pcm",
            "-fmodule-mapper=m.txt",
            "-fprebuilt-module-path=pcm",
            "-fdeps-file=x.ddi",
            "-fdeps-format=p1689r5",
            "--precompile",
        ] {
            assert!(refuses(&[arg]), "{arg}");
        }
        for arg in [
            "-fopt-info",
            "-fopt-info-vec",
            "-gsplit-dwarfx",
            "-save-tempsx",
            "-fstack-usage=x",
            "-ftest-coverage=x",
            "-fdiagnostics-format=sarif",
            "-fmodules-cache-path=x",
            "-fmodule-name=x",
            "-Wa,-adhln",
            "-Wa,--gdwarf-5",
            "-Wl,-a=x",
            "-fdump",
            "-coverage",
            "-g",
        ] {
            assert!(!refuses(&[arg]), "{arg}");
        }
    }

    #[test]
    fn rule_b_hidden_inputs() {
        for args in [
            &["-fprofile-use"][..],
            &["-fprofile-use=/p"],
            &["-fprofile-instr-use=x.profdata"],
            &["-fprofile-sample-use=x.afdo"],
            &["-fauto-profile"],
            &["-fauto-profile=x.afdo"],
            &["-fplugin=p.so"],
            &["-fpass-plugin=p.so"],
            &["-specs=x.specs"],
            &["--specs=x.specs"],
            &["-Xclang", "-load", "-Xclang", "p.so"],
            &["-O2", "-Xclang", "-plugin", "-Xclang", "p"],
        ] {
            assert!(refuses(args), "{args:?}");
        }
        for args in [
            &["-fprofile-usex"][..],
            &["-fprofile-instr-use"],
            &["-fprofile-generate"],
            &["-Xclang", "-loadx"],
            &["-Xclang"],
            &["-load", "-Xclang"],
            &["-fplugin-arg-x=y"],
        ] {
            assert!(!refuses(args), "{args:?}");
        }
    }

    #[test]
    fn rule_d_foreign_temp_dirs() {
        for args in [
            &["-I", "/build/go-build123/b001"][..],
            &["-I/tmp/go-build4"],
            &["/build/ghc1234_0/ghc_1.c"],
            &["-c", "/tmp/tmpxft_00001_00000-6_x.cudafe1.cpp"],
            &["/build/camlasm7f.s"],
            &["camlasm"],
            &["-o/tmp/go-build1/x.o"],
        ] {
            assert!(refuses(args), "{args:?}");
        }
        for args in [
            &["-I/build/go-build"][..],
            &["-I/build/go-buildx1"],
            &["-I/build/xgo-build1"],
            &["/build/ghc12_/x.c"],
            &["/build/ghc_1/x.c"],
            &["/build/ghcx1_2/x.c"],
            &["/build/xtmpxft_1.c"],
            &["-I/build/mycamlasm"],
        ] {
            assert!(!refuses(args), "{args:?}");
        }
        assert!(adopted_rules_refuse(&["-c"], b"/build/ghc12_3/ghc_1.c"));
        assert!(!adopted_rules_refuse(&["-c"], b"/build/src/x.c"));
    }

    #[test]
    fn rule_c_incbin() {
        assert!(mentions_incbin(b"  .incbin \"data.bin\"\n"));
        assert!(mentions_incbin(b"# .incbin in a comment\n"));
        assert!(!mentions_incbin(b".include \"x.inc\"\n"));
    }
}
