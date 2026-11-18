//! `wire::parse_request` and the number parsers: no panic; on the inputs of
//! `tests/vectors/go_parseint.json`, `parse_int64` is Go's `strconv.ParseInt(s, 10, 64)`.
#![no_main]

#[path = "common.rs"]
mod common;

use std::collections::HashMap;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;

static GO: LazyLock<HashMap<Vec<u8>, serde_json::Value>> =
    LazyLock::new(|| common::oracle(include_str!("../../tests/vectors/go_parseint.json")));

fuzz_target!(|line: &[u8]| {
    let _ = jig_core::wire::parse_request(line);
    let _ = jig_core::wire::parse_ids_count(line);
    let _ = jig_core::wire::parse_hello_ver(line);
    let n = jig_core::wire::parse_int64(line);
    if let Some(row) = GO.get(line) {
        assert_eq!(n, row["int64"].as_i64(), "Go ParseInt disagrees");
    }
});
