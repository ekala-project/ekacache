//! `DriverConf`: `etc/jig.json`, `$PKGS_CC`, `$JIG_CC` (`behaviors/jig-driver-policy.md
//! §Configuration`, `driver.cc:350-430`).

use jig_core::bytes::find;
use jig_core::fspath;

use super::json::{self, Value};
use crate::process::ProcEnv;
use crate::util::read_file;

/// The package's own flags from `$PKGS_CC` (`PackageCcFlags`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageCcFlags {
    /// Every compile and link, FORTIFY-filtered (A4).
    pub cflags: Vec<Vec<u8>>,
    /// C++ only.
    pub cxxflags: Vec<Vec<u8>>,
    /// Link steps only, after the user args (A5).
    pub ldflags: Vec<Vec<u8>>,
}

/// `BinFmt`: which per-format policy applies (A1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BinFmt {
    #[default]
    Elf,
    MachO,
    Coff,
}

/// `DriverConf` with the documented defaults (§Interface to cc mode).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverConf {
    pub cc: Vec<u8>,
    pub fc: Vec<u8>,
    pub binfmt: BinFmt,
    pub libc: Vec<u8>,
    pub interp: Vec<u8>,
    pub crt: Vec<u8>,
    pub runtimes: Vec<u8>,
    pub flags: Vec<Vec<u8>>,
    pub cxxflags: Vec<Vec<u8>>,
    pub fflags: Vec<Vec<u8>>,
    pub prefix_map: Vec<Vec<u8>>,
    pub package: PackageCcFlags,
    /// `false`: conf-less, only `cc` is set.
    pub present: bool,
}

impl Default for DriverConf {
    fn default() -> Self {
        Self {
            cc: Vec::new(),
            fc: Vec::new(),
            binfmt: BinFmt::Elf,
            libc: Vec::new(),
            interp: b"ld-linux-x86-64.so.2".to_vec(),
            crt: Vec::new(),
            runtimes: Vec::new(),
            flags: Vec::new(),
            cxxflags: Vec::new(),
            fflags: Vec::new(),
            prefix_map: Vec::new(),
            package: PackageCcFlags::default(),
            present: false,
        }
    }
}

/// How reading the conf ends the process (§Errors and messages).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fatal {
    /// The message (with its `\n`) on stderr, then `exit(2)`.
    Exit(Vec<u8>),
    /// SIGABRT, no message: nlohmann's `get<std::vector<std::string>>()` on a wrong type
    /// (Q-DRV-9).
    Abort,
}

impl Fatal {
    /// End the process the way legacy does.
    pub fn die(self) -> ! {
        match self {
            Fatal::Exit(msg) => {
                use std::io::Write;
                let _ = std::io::stderr().write_all(&msg);
                std::process::exit(2)
            }
            // Q-DRV-9
            Fatal::Abort => std::process::abort(),
        }
    }
}

/// `get<std::vector<std::string>>()`: an array of strings, anything else aborts.
fn string_list(v: &Value) -> Result<Vec<Vec<u8>>, Fatal> {
    v.string_list().ok_or(Fatal::Abort)
}

/// `ParseDriverConf(text, root)`. Keys are read in the documented order so that an abort from a
/// bad array comes before a bad `binfmt` message.
pub fn parse_driver_conf(text: &[u8], root: &[u8]) -> Result<DriverConf, Fatal> {
    let doc = match json::parse(text) {
        Some(doc) if doc.is_object() => doc,
        _ => return Err(Fatal::Exit(b"jig.json: not a JSON object\n".to_vec())),
    };
    let mut conf = DriverConf {
        present: true,
        ..DriverConf::default()
    };
    let str_key = |key: &[u8], into: &mut Vec<u8>| {
        // a value of another type is ignored, the default stays
        if let Some(s) = doc.get(key).and_then(Value::as_string) {
            *into = s.to_vec();
        }
    };
    str_key(b"cc", &mut conf.cc);
    str_key(b"fc", &mut conf.fc);
    str_key(b"libc", &mut conf.libc);
    str_key(b"interp", &mut conf.interp);
    str_key(b"crt", &mut conf.crt);
    str_key(b"runtimes", &mut conf.runtimes);
    for (key, into) in [
        (&b"flags"[..], &mut conf.flags),
        (b"cxxflags", &mut conf.cxxflags),
        (b"fflags", &mut conf.fflags),
        (b"prefix-map", &mut conf.prefix_map),
    ] {
        if let Some(v) = doc.get(key).filter(|v| v.as_array().is_some()) {
            *into = string_list(v)?;
        }
    }
    let mut binfmt = b"elf".to_vec();
    str_key(b"binfmt", &mut binfmt);
    conf.binfmt = match &binfmt[..] {
        b"elf" => BinFmt::Elf,
        b"macho" => BinFmt::MachO,
        b"coff" => BinFmt::Coff,
        other => {
            let mut msg = b"jig.json: binfmt ".to_vec();
            msg.extend_from_slice(other);
            msg.extend_from_slice(b" is none of elf, macho, coff\n");
            return Err(Fatal::Exit(msg));
        }
    };
    // "@/" at the start, after '=' or after a short option expands to jig's own prefix
    // (§`@/` expansion in `fflags`, Q-DRV-13: the first occurrence only)
    for flag in &mut conf.fflags {
        let Some(p) = find(flag, b"@/") else { continue };
        let expands = p == 0 || (p == 2 && flag.first() == Some(&b'-')) || flag[p - 1] == b'=';
        if expands {
            flag.splice(p..=p, root.iter().copied());
        }
    }
    Ok(conf)
}

/// `PackageFlags(root)` over the value of `$PKGS_CC`.
pub fn package_flags(pkgs_cc: &[u8], root: &[u8]) -> Result<PackageCcFlags, Fatal> {
    let mut flags = PackageCcFlags::default();
    if pkgs_cc.is_empty() {
        return Ok(flags);
    }
    let Some(all) = json::parse(pkgs_cc) else {
        return Ok(flags);
    };
    // no normalisation: the key must be byte-equal to the root (Q-DRV-2)
    let Some(entry) = all.get(root) else {
        return Ok(flags);
    };
    for (key, into) in [
        (&b"cflags"[..], &mut flags.cflags),
        (b"cxxflags", &mut flags.cxxflags),
        (b"ldflags", &mut flags.ldflags),
    ] {
        if let Some(v) = entry.get(key) {
            *into = string_list(v)?;
        }
    }
    Ok(flags)
}

/// `ConfLessDriverConf(compiler)`: only the compiler, and the build host's binary format.
#[must_use]
pub fn conf_less(compiler: &[u8]) -> DriverConf {
    DriverConf {
        cc: compiler.to_vec(),
        binfmt: if cfg!(target_vendor = "apple") {
            BinFmt::MachO
        } else {
            BinFmt::Elf
        },
        ..DriverConf::default()
    }
}

/// `LoadDriverConf()` for an executable path (`None`: `/proc/self/exe` unreadable).
pub fn load_driver_conf_for(
    exe: Option<&[u8]>,
    env: &ProcEnv,
) -> Result<Option<DriverConf>, Fatal> {
    if let Some(exe) = exe {
        let root = fspath::parent_path(&fspath::parent_path(exe));
        if let Some(text) = read_file(&fspath::join(&root, b"etc/jig.json")) {
            let mut conf = parse_driver_conf(&text, &root)?;
            // Q-DRV-1: a conf without `cc` does not fall back to $JIG_CC
            if conf.cc.is_empty() {
                return Ok(None);
            }
            conf.package = package_flags(env.get_or("PKGS_CC", b""), &root)?;
            return Ok(Some(conf));
        }
    }
    Ok(env.non_empty("JIG_CC").map(conf_less))
}

/// `LoadDriverConf()`: `<exe>/../../etc/jig.json` (Q-DRV-2: located through `/proc/self/exe`
/// only), else `$JIG_CC`. Exits or aborts on a broken conf.
#[must_use]
pub fn load_driver_conf(env: &ProcEnv) -> Option<DriverConf> {
    let exe = std::fs::read_link("/proc/self/exe")
        .ok()
        .map(|p| p.into_os_string().into_encoded_bytes());
    load_driver_conf_for(exe.as_deref(), env).unwrap_or_else(|f| f.die())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jig_core::env::Env;

    fn v(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    const ELF_CONF: &str = r#"{"cc": "/seed/bin/clang", "flags": ["--target=x", "-O2"], "cxxflags": ["-stdlib=libc++"], "libc": "/sr/libc", "crt": "/cc/lib/crt_interp.o", "runtimes": "/sr/rt/lib"}"#;

    #[test]
    fn tv1_tv2_parse() {
        let c = parse_driver_conf(ELF_CONF.as_bytes(), b"").unwrap();
        assert!(c.present);
        assert_eq!(c.cc, b"/seed/bin/clang");
        assert_eq!(c.flags, v(&["--target=x", "-O2"]));
        assert_eq!(c.interp, b"ld-linux-x86-64.so.2");
        let c = parse_driver_conf(
            br#"{"cc": "/c", "fc": "/f/bin/flang", "fflags": ["-L@/lib", "-fintrinsic-modules-path", "@/finc", "-resource-dir=@/rd", "-Da@/b"]}"#,
            b"/self",
        )
        .unwrap();
        assert_eq!(c.fc, b"/f/bin/flang");
        assert_eq!(
            c.fflags,
            v(&[
                "-L/self/lib",
                "-fintrinsic-modules-path",
                "/self/finc",
                "-resource-dir=/self/rd",
                "-Da@/b"
            ])
        );
    }

    #[test]
    fn tv30_at_expansion() {
        let c = parse_driver_conf(
            br#"{"cc":"/c","fc":"/f","fflags":["-L@/lib","@/a@/b","--@/x","a=b@/c=@/d","x@/y","-I","@/i"],"flags":["-fc"],"prefix-map":["/b=/c"],"libc":"/l"}"#,
            b"/self",
        )
        .unwrap();
        assert_eq!(
            c.fflags,
            v(&[
                "-L/self/lib",
                "/self/a@/b",
                "--/self/x",
                "a=b@/c=@/d",
                "x@/y",
                "-I",
                "/self/i"
            ])
        );
        let c = parse_driver_conf(br#"{"fflags":["@/lib"]}"#, b"").unwrap();
        assert_eq!(c.fflags, v(&["/lib"]));
    }

    /// Beyond TV-36, which `tests/driver_vectors.rs` reads from
    /// `tests/vectors/jig-driver-policy.toml`.
    #[test]
    fn empty_interp_and_no_cc() {
        let c = parse_driver_conf(br#"{"interp":"","binfmt":"coff"}"#, b"").unwrap();
        assert_eq!(c.interp, b"");
        assert_eq!(c.binfmt, BinFmt::Coff);
        assert!(c.cc.is_empty());
    }

    #[test]
    fn pkgs_cc() {
        let root = b"/r";
        let ok = |t: &str| package_flags(t.as_bytes(), root).unwrap();
        // TV-37's rows are in `tests/driver_vectors.rs`
        for t in ["", r#"{"/r":{}}"#] {
            assert_eq!(ok(t), PackageCcFlags::default(), "{t}");
        }
        let f = ok(r#"{"/r":{"cflags":["-DP"],"ldflags":["-L/dep/lib","-L","/x"]}}"#);
        assert_eq!(f.cflags, v(&["-DP"]));
        assert_eq!(f.ldflags, v(&["-L/dep/lib", "-L", "/x"]));
        assert!(f.cxxflags.is_empty());
        assert_eq!(
            package_flags(br#"{"/r":{"ldflags":["a",2]}}"#, root),
            Err(Fatal::Abort)
        );
    }

    #[test]
    fn locating() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path().as_os_str().as_encoded_bytes().to_vec();
        let mut exe = r.clone();
        exe.extend_from_slice(b"/bin/jig");
        let env = |pairs: &[(&str, &str)]| ProcEnv::new(Env::from_pairs(pairs));
        // no conf, no JIG_CC
        assert_eq!(load_driver_conf_for(Some(&exe), &env(&[])), Ok(None));
        assert_eq!(
            load_driver_conf_for(Some(&exe), &env(&[("JIG_CC", "")])),
            Ok(None)
        );
        let c = load_driver_conf_for(None, &env(&[("JIG_CC", "/bin/cc")]))
            .unwrap()
            .unwrap();
        assert_eq!(c, conf_less(b"/bin/cc"));
        assert!(!c.present);
        // the build host's format (`driver.cc:439`): Mach-O on Apple, so conf-less links there
        // are uncacheable (`jig-driver-policy.md §$JIG_CC and conf-less mode`)
        let host = if cfg!(target_vendor = "apple") {
            BinFmt::MachO
        } else {
            BinFmt::Elf
        };
        assert_eq!(c.binfmt, host);
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        // Q-DRV-1: a conf without cc ignores JIG_CC
        std::fs::write(dir.path().join("etc/jig.json"), r#"{"flags":[]}"#).unwrap();
        assert_eq!(
            load_driver_conf_for(Some(&exe), &env(&[("JIG_CC", "/bin/true")])),
            Ok(None)
        );
        std::fs::write(dir.path().join("etc/jig.json"), r#"{"cc":"/c"}"#).unwrap();
        let pkgs = format!(r#"{{"{}":{{"cflags":["-DP"]}}}}"#, dir.path().display());
        let c = load_driver_conf_for(Some(&exe), &env(&[("PKGS_CC", &pkgs)]))
            .unwrap()
            .unwrap();
        assert_eq!(c.package.cflags, v(&["-DP"]));
        assert!(c.present);
    }
}
