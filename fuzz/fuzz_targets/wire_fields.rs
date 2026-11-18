//! `wire::fields`: no panic; it agrees with the iterator (count, and the first four fields it
//! keeps); every field is non-empty; on the inputs of `tests/vectors/go_fields.json`, the result
//! is Go's `strings.Fields`.
#![no_main]

#[path = "common.rs"]
mod common;

use std::collections::HashMap;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;

static GO: LazyLock<HashMap<Vec<u8>, serde_json::Value>> =
    LazyLock::new(|| common::oracle(include_str!("../../tests/vectors/go_fields.json")));

fuzz_target!(|line: &[u8]| {
    let f = jig_core::wire::fields(line);
    let iterated: Vec<&[u8]> = jig_core::wire::field_iter(line).collect();
    assert_eq!(f.len(), iterated.len());
    let kept: Vec<&[u8]> = (0..f.len().min(4))
        .map(|i| f.get(i).expect("kept"))
        .collect();
    assert_eq!(kept, iterated[..kept.len()]);
    assert!(iterated.iter().all(|w| !w.is_empty()));
    if let Some(row) = GO.get(line) {
        let want: Vec<Vec<u8>> = row["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .map(|w| common::unhex(w.as_str().expect("hex")))
            .collect();
        assert_eq!(iterated, want, "Go strings.Fields disagrees");
    }
});
