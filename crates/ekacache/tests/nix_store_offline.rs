//! `jig nix-store` with `JIG_NIX_STORE_OFFLINE` (`behaviors/jig-nix-store.md §Test vectors`
//! [bin] and `§CLI behavior`, `§Derivations`, Q-NIX-3/4/5/13/14/16/17), compared with the legacy
//! jig when `JIG_LEGACY_JIG` is set. No daemon is ever contacted.
#![allow(clippy::unwrap_used)]

mod nix_store_support;

use ekacache_testkit::vectors::VectorFile;

use nix_store_support::{
    Inv, Outcome, Remote, aterm, aterm_json, bad_arguments, check, check_offline, did_not_parse,
    fetch_one, fetch_one_aterm, fetch_one_out, not_an_array, usage,
};

fn offline(args: &[&[u8]], stdin: &[u8]) -> Outcome {
    check_offline(&Inv::new(args).stdin(stdin))
}

fn vectors() -> VectorFile {
    VectorFile::load("jig-nix-store")
}

/// §Text paths [bin]: the `text-path` rows of `tests/vectors/jig-nix-store.toml`.
#[test]
fn text_path_vectors() {
    for v in vectors().of_kind("text-path", 6) {
        let mut args: Vec<&[u8]> = vec![b"add-text", v.bytes("name")];
        let refs = v.byte_strs("refs");
        args.extend(refs.iter().map(Vec::as_slice));
        offline(&args, v.bytes("stdin")).assert_prints(v.bytes("path"));
    }
}

/// Q-NIX-16: names, refs and contents are not validated; stdin is raw bytes.
#[test]
fn text_paths_take_anything() {
    let binary: Vec<u8> = (0..=255u8).chain([b'\r', b'\n', 0]).collect();
    let rows: &[&[&[u8]]] = &[
        &[b"add-text", b"a/b", b"not a path", b"\xff\xfe"],
        &[b"add-text", b"\n", b"", b""],
        &[b"add-drv", b"x"],
    ];
    for args in rows {
        let o = offline(args, &binary);
        if args[0] == b"add-drv" {
            o.assert_fails(1, &[did_not_parse()]);
        } else {
            assert_eq!(o.code, Some(0), "{o:?}");
        }
    }
}

/// §CLI behavior [bin, offline]: the `cli` rows, then cases beyond the doc's table.
#[test]
fn cli_behaviour() {
    for v in vectors().of_kind("cli", 12) {
        let at = v.label();
        let args: Vec<&[u8]> = v.strs("args").into_iter().map(str::as_bytes).collect();
        let mut inv = Inv::new(&args).stdin(v.opt_bytes("stdin").unwrap_or(b""));
        let env = v.opt_strs("env").unwrap_or_default();
        for kv in &env {
            let (k, val) = kv.split_once('=').unwrap();
            inv = inv.env(k, val.as_bytes());
        }
        let o = if env.is_empty() {
            check_offline(&inv)
        } else {
            check(&inv, &[])
        };
        assert!(!o.connected, "{at}");
        let line = |key: &str| {
            v.opt_bytes(key)
                .map(|l| [l, b"\n"].concat())
                .unwrap_or_default()
        };
        assert_eq!(
            o.code,
            Some(i32::try_from(v.int("status")).unwrap()),
            "{at}: {o:?}"
        );
        assert_eq!(o.stderr, line("stderr"), "{at}: {o:?}");
        assert_eq!(o.stdout, line("stdout"), "{at}: {o:?}");
    }
    let rows: &[&[&[u8]]] = &[
        &[b"add-drv"],
        &[b"submit"],
        &[b"submit", b"a", b"b", b"c"],
        &[b"--help"],
        &[b"-h"],
        &[b"ADD-TEXT", b"x"],
    ];
    for args in rows {
        offline(args, b"").assert_fails(2, &[bad_arguments()]);
    }
    // Q-NIX-14
    let o = offline(&[b"submit", b"", b""], b"");
    o.assert_fails(0, &[]);
    for stdin in [&b""[..], b"null", b"\"[]\""] {
        offline(&[b"fetchurls"], stdin).assert_fails(1, &[not_an_array()]);
    }
    offline(&[b"fetchurls"], b" \t\r\n[ ]\n").assert_prints(b"[]");
}

/// No arguments: usage, before any environment is read (no `NIX_REMOTE` message, no connection).
#[test]
fn usage_reads_nothing() {
    for remote in [
        Remote::Unset,
        Remote::Daemon,
        Remote::Value(b"daemon".to_vec()),
    ] {
        let o = check(&Inv::new::<&[u8]>(&[]).remote(remote), &[]);
        o.assert_fails(2, &[usage()]);
        assert!(!o.connected);
    }
}

/// Q-NIX-3: any non-empty value means offline.
#[test]
fn offline_is_a_non_empty_test() {
    for value in [&b"1"[..], b"0", b"false", b"no", b" ", b"\xff"] {
        let inv = Inv::new(&[&b"add-text"[..], b"hello.txt"])
            .stdin(b"hello")
            .env("JIG_NIX_STORE_OFFLINE", value);
        let o = check(&inv, &[]);
        assert!(!o.connected);
        o.assert_prints(b"/nix/store/q790zdjk75hm2cn42nh77pqw4gbv1b88-hello.txt");
    }
    // offline needs no NIX_REMOTE at all
    let inv = Inv::new(&[&b"submit"[..], b"p", b"o"])
        .remote(Remote::Unset)
        .env("JIG_NIX_STORE_OFFLINE", b"0");
    check(&inv, &[]).assert_fails(0, &[]);
}

/// The store path the Rust jig gives `aterm` as a text object (checked by the vectors above).
fn text_path(name: &[u8], refs: &[&[u8]], aterm: &[u8]) -> Vec<u8> {
    let mut args: Vec<&[u8]> = vec![b"add-text", name];
    args.extend_from_slice(refs);
    let o = nix_store_support::Running::start(
        std::path::Path::new(nix_store_support::JIG),
        &Inv::new(&args).stdin(aterm).offline(),
        &[],
        tempfile::tempdir().unwrap().path(),
    )
    .finish();
    assert_eq!(o.code, Some(0));
    o.stdout.strip_suffix(b"\n").unwrap().to_vec()
}

/// `add-drv`'s path is the text path of the expected ATerm.
#[track_caller]
fn assert_aterm(json: &[u8], aterm: &[u8]) {
    let o = offline(&[b"add-drv", b"x.drv"], json);
    o.assert_prints(&text_path(b"x.drv", &[], aterm));
}

/// §ATerm [test]: the paths of the vector, with and without references (Q-NIX-13: from argv).
#[test]
fn aterm_vector() {
    let file = vectors();
    let v = file.one("aterm");
    offline(&[b"add-drv", b"x.drv"], aterm_json()).assert_prints(v.bytes("drv_path"));
    let mut args: Vec<&[u8]> = vec![b"add-drv", b"x.drv"];
    let refs = v.byte_strs("refs");
    args.extend(refs.iter().map(Vec::as_slice));
    offline(&args, aterm_json()).assert_prints(v.bytes("drv_path_with_refs"));
    assert_eq!(text_path(b"x.drv", &[], aterm()), v.bytes("drv_path"));
    // the JSON "name" is ignored: the store name is the argument
    offline(&[b"add-drv", b"y"], aterm_json()).assert_prints(&text_path(b"y", &[], aterm()));
}

/// §JSON decoding and §add-drv input → ATerm: shapes that encode.
#[test]
fn aterm_shapes() {
    const EMPTY: &[u8] = br#"Derive([],[],[],"","",[],[])"#;
    let rows: &[(&[u8], &[u8])] = &[
        (b"{}", EMPTY),
        (
            br#"{"outputs":null,"inputDrvs":null,"env":null,"name":5}"#,
            EMPTY,
        ),
        // iteration of a scalar: key ""; of an array: index keys
        (br#"{"env":"x"}"#, br#"Derive([],[],[],"","",[],[("","x")])"#),
        (
            br#"{"env":["a","b"]}"#,
            br#"Derive([],[],[],"","",[],[("0","a"),("1","b")])"#,
        ),
        (
            br#"{"outputs":{"out":{}},"inputDrvs":{"/d":[]}}"#,
            br#"Derive([("out","","","")],[("/d",[])],[],"","",[],[])"#,
        ),
        (
            br#"{"outputs":{"out":{"path":"/p","hash":"h","x":1}}}"#,
            br#"Derive([("out","/p","","h")],[],[],"","",[],[])"#,
        ),
        // Q-NIX-5: sorted, duplicates kept; args keep their order
        (
            br#"{"inputDrvs":{"/e":["b","a","a"],"/d":["x"]},"inputSrcs":["/b","/a","/a"],"args":["b","a","b"]}"#,
            br#"Derive([],[("/d",["x"]),("/e",["a","a","b"])],["/a","/a","/b"],"","",["b","a","b"],[])"#,
        ),
        // bytewise key order
        (
            "{\"env\":{\"b\":\"1\",\"B\":\"2\",\"é\":\"3\",\"\":\"4\"}}".as_bytes(),
            "Derive([],[],[],\"\",\"\",[],[(\"\",\"4\"),(\"B\",\"2\"),(\"b\",\"1\"),(\"é\",\"3\")])"
                .as_bytes(),
        ),
        // Q(s) escapes five bytes; NUL and other controls are verbatim
        (
            br#"{"system":"a\"b\\c\nd\re\tf\u0001g\u0000h\u00e9\/"}"#,
            b"Derive([],[],[],\"a\\\"b\\\\c\\nd\\re\\tf\x01g\x00h\xc3\xa9/\",\"\",[],[])",
        ),
        // the last duplicate key wins
        (
            br#"{"system":"a","system":"b","env":{"k":"1","k":"2"}}"#,
            br#"Derive([],[],[],"b","",[],[("k","2")])"#,
        ),
        // a leading BOM; NUL outside a string ends the input
        (b"\xef\xbb\xbf {}", EMPTY),
        (b"{}\x00junk", EMPTY),
        (b"{\"system\":\"s\"}\x00{", br#"Derive([],[],[],"s","",[],[])"#),
        // numbers, literals and surrogate pairs in ignored keys
        (
            br#"{"n":[0,-0,1.5e3,-1E-2,18446744073709551615,18446744073709551616,1e308,true,false,null],"s":"\ud83d\ude00"}"#,
            EMPTY,
        ),
    ];
    for (json, aterm) in rows {
        assert_aterm(json, aterm);
    }
}

/// Deep nesting in a key the encoder never copies parses (nlohmann has no depth limit).
#[test]
fn aterm_deep_nesting() {
    let depth = 100_000;
    let mut json = b"{\"x\":".to_vec();
    json.extend(std::iter::repeat_n(b"[{\"y\":".as_slice(), depth).flatten());
    json.push(b'0');
    json.extend(std::iter::repeat_n(b"}]".as_slice(), depth).flatten());
    json.extend_from_slice(br#","system":"s"}"#);
    assert_aterm(&json, br#"Derive([],[],[],"s","",[],[])"#);
    // unbalanced at depth: a parse error
    let cut = &json[..json.len() - 20];
    offline(&[b"add-drv", b"x.drv"], cut).assert_fails(1, &[did_not_parse()]);
}

/// Parse failures, a non-object top level and non-string `env` values: the clean failure.
#[test]
fn aterm_failures() {
    let rows: &[&[u8]] = &[
        b"",
        b" ",
        b"abc",
        b"[]",
        b"null",
        b"1",
        b"\"{}\"",
        b"{} x",
        b"{}{}",
        b"{\"a\":1,}",
        b"{\"a\":1}//c",
        b"/* c */{}",
        b"{'a':1}",
        b"{\"a\":01}",
        b"{\"a\":1.}",
        b"{\"a\":.5}",
        b"{\"a\":+1}",
        b"{\"a\":NaN}",
        b"{\"a\":1e400}",
        b"{\"a\":tru}",
        b"{\"a\":\"\xff\"}",
        b"{\"a\":\"\xc0\xaf\"}",
        b"{\"a\":\"\xed\xa0\x80\"}",
        b"{\"a\":\"\\ud800\"}",
        b"{\"a\":\"\\udc00\\ud800\"}",
        b"{\"a\":\"\\x\"}",
        b"{\"a\":\"\t\"}",
        b"\xef\xbb{}",
        b"\xef\xbb\xbf\xef\xbb\xbf{}",
        b"\x00{}",
        b"{\"a\x00\":1}",
        // env (step 4) values must be strings: failure, not abort
        br#"{"env":{"a":1}}"#,
        br#"{"env":{"a":null}}"#,
        br#"{"env":{"a":{}}}"#,
        br#"{"env":[1]}"#,
        br#"{"env":5}"#,
        br#"{"env":true}"#,
        // decided before the later fields are looked at
        br#"{"env":{"a":1},"args":1,"system":1,"builder":1}"#,
        br#"{"env":{"a":1},"outputs":{"o":1}}"#,
    ];
    for json in rows {
        offline(&[b"add-drv", b"x.drv"], json).assert_fails(1, &[did_not_parse()]);
    }
}

/// Q-NIX-4: every other type error aborts.
#[test]
fn aterm_aborts() {
    let rows: &[&[u8]] = &[
        br#"{"system":1}"#,
        br#"{"system":null}"#,
        br#"{"builder":[]}"#,
        br#"{"builder":{}}"#,
        br#"{"args":null}"#,
        br#"{"args":"a"}"#,
        br#"{"args":{}}"#,
        br#"{"args":[1]}"#,
        br#"{"args":["a",null]}"#,
        br#"{"inputSrcs":null}"#,
        br#"{"inputSrcs":"x"}"#,
        br#"{"inputSrcs":[1]}"#,
        br#"{"inputDrvs":{"/d":"out"}}"#,
        br#"{"inputDrvs":{"/d":[1]}}"#,
        br#"{"inputDrvs":{"/d":null}}"#,
        br#"{"inputDrvs":[1]}"#,
        br#"{"inputDrvs":"x"}"#,
        br#"{"outputs":{"out":1}}"#,
        br#"{"outputs":{"out":null}}"#,
        br#"{"outputs":{"out":[]}}"#,
        br#"{"outputs":{"out":{"path":1}}}"#,
        br#"{"outputs":{"out":{"hashAlgo":null}}}"#,
        br#"{"outputs":{"out":{"hash":[]}}}"#,
        br#"{"outputs":[1]}"#,
        br#"{"outputs":"x"}"#,
        // inputSrcs and inputDrvs (steps 2, 3) come before env (step 4)
        br#"{"env":{"a":1},"inputSrcs":1}"#,
        br#"{"env":{"a":1},"inputDrvs":{"d":1}}"#,
    ];
    for json in rows {
        offline(&[b"add-drv", b"x.drv"], json).assert_killed(libc::SIGABRT);
    }
}

/// §fetchurls [derived].
#[test]
fn fetchurls_vector() {
    let file = vectors();
    let v = file.one("fetchurls");
    offline(&[b"fetchurls"], fetch_one()).assert_prints(fetch_one_out());
    assert_eq!(
        text_path(v.bytes("drv_name"), &[], fetch_one_aterm()),
        v.bytes("drv")
    );
    assert!(
        fetch_one_out()
            .windows(v.bytes("drv").len())
            .any(|w| w == v.bytes("drv"))
    );
    // duplicates give duplicate entries, in order
    let two = [
        &fetch_one()[..fetch_one().len() - 1],
        b",",
        &fetch_one()[1..],
    ]
    .concat();
    let out = [
        &fetch_one_out()[..fetch_one_out().len() - 1],
        b",",
        &fetch_one_out()[1..],
    ]
    .concat();
    offline(&[b"fetchurls"], &two).assert_prints(&out);
}

/// The fetchurl derivation's edge cases: missing fields, `outputHashAlgo`, `dump()` escaping.
#[test]
fn fetchurls_shapes() {
    for json in [
        &b"[{}]"[..],
        br#"[{"name":"a","algo":"sha256","sri":"sha512-x","hex":"00","x":[1,{"y":null}]}]"#,
        br#"[{"name":"a","algo":"","sri":"-x"}]"#,
        br#"[{"name":"a","algo":"sha256","sri":"sha256"}]"#,
        br#"[{"name":"a","algo":"r:sha256","url":"u\"\\\n"}]"#,
        "[{\"name\":\"q\\\"\\\\\\u0001\\u001f\\u007f\\b\\f\\t\\n\\r/é\"},{\"name\":\"b\"}]"
            .as_bytes(),
        b"\xef\xbb\xbf[{\"name\":\"a\"}]\x00x",
    ] {
        let o = offline(&[b"fetchurls"], json);
        assert_eq!(o.code, Some(0), "{o:?}");
        assert!(o.stdout.starts_with(b"[{\"drv\":\"/nix/store/"), "{o:?}");
    }
    // `oha` is empty when `sri` starts with `algo-`, else `algo`: the .drv paths differ
    let drv = |json: &[u8]| offline(&[b"fetchurls"], json).stdout;
    assert_ne!(
        drv(br#"[{"name":"a","algo":"sha256","sri":"sha256-x"}]"#),
        drv(br#"[{"name":"a","algo":"sha256","sri":"sha512-x"}]"#)
    );
    // the escaping of a name with control bytes in the printed JSON
    let o = offline(&[b"fetchurls"], br#"[{"name":"\u0001\b\"\u007f"}]"#);
    let text = String::from_utf8(o.stdout).unwrap();
    assert!(
        text.contains("-\\u0001\\b\\\"\x7f.drv\",\"out\":"),
        "{text}"
    );
}

/// Q-NIX-4 in fetchurls: a non-object element or a non-string field aborts, also after earlier
/// elements succeeded (Q-NIX-17: nothing on stdout).
#[test]
fn fetchurls_aborts() {
    for json in [
        &b"[1]"[..],
        b"[null]",
        b"[[]]",
        b"[\"x\"]",
        br#"[{"name":1}]"#,
        br#"[{"name":null}]"#,
        br#"[{"algo":1}]"#,
        br#"[{"hex":[]}]"#,
        br#"[{"sri":1}]"#,
        br#"[{"url":{}}]"#,
        br#"[{"name":"a"},1]"#,
        br#"[{"name":"a"},{"url":false}]"#,
    ] {
        offline(&[b"fetchurls"], json).assert_killed(libc::SIGABRT);
    }
}
