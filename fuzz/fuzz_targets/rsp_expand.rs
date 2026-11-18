//! `util::expand_response_files`: a response file with arbitrary contents; no panic, and the
//! expansion is not recursive (an `@word` inside the file stays a word).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|body: &[u8]| {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    std::io::Write::write_all(&mut file, body).expect("write");
    let mut arg = b"@".to_vec();
    arg.extend_from_slice(file.path().as_os_str().as_encoded_bytes());
    let words = jig::util::expand_response_files(&[arg.as_slice(), b"-c"]);
    assert_eq!(words.last().map(Vec::as_slice), Some(&b"-c"[..]));
});
