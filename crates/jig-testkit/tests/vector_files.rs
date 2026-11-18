//! Every vector heading in `behaviors/` has its data file (`implementation/12-testing.md §L1:
//! spec vectors`): a doc with a heading matching `/vector/i` has `tests/vectors/<doc>.toml`,
//! each such heading is named by at least one entry's `section`, and every entry's `section`
//! is a real heading path in its doc. Run by `scripts/check-vectors.sh`.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use jig_testkit::vectors::{
    DOC_ONLY, VectorFile, behaviors_dir, headings, is_vector_heading, section_exists, vectors_dir,
};

/// The `<doc>.toml` files under `tests/vectors/` (other files there are not L1 vectors).
fn vector_files() -> BTreeSet<String> {
    std::fs::read_dir(vectors_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter_map(|n| n.strip_suffix(".toml").map(str::to_owned))
        .collect()
}

fn doc(stem: &str) -> Option<String> {
    std::fs::read_to_string(behaviors_dir().join(format!("{stem}.md"))).ok()
}

#[test]
fn every_vector_heading_has_a_file_and_an_entry() {
    let files = vector_files();
    let mut problems = Vec::new();
    let mut docs: Vec<_> = std::fs::read_dir(behaviors_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    docs.sort();
    for path in docs {
        let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let hs = headings(&text);
        let vector_headings: Vec<_> = hs.iter().filter(|h| is_vector_heading(h)).collect();
        if vector_headings.is_empty() {
            continue;
        }
        if !files.contains(&stem) {
            problems.push(format!(
                "behaviors/{stem}.md has vector headings but no tests/vectors/{stem}.toml"
            ));
            continue;
        }
        let file = VectorFile::load(&stem);
        let named: BTreeSet<&str> = file
            .vectors
            .iter()
            .flat_map(|v| v.section().split(" > ").map(str::trim))
            .collect();
        for h in vector_headings {
            if !named.contains(h.text.as_str()) {
                problems.push(format!(
                    "behaviors/{stem}.md: heading {:?} is named by no entry of tests/vectors/{stem}.toml",
                    h.text
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn every_entry_names_a_real_section() {
    let mut problems = Vec::new();
    for stem in vector_files() {
        let Some(text) = doc(&stem) else {
            problems.push(format!("tests/vectors/{stem}.toml: no behaviors/{stem}.md"));
            continue;
        };
        let hs = headings(&text);
        let file = VectorFile::load(&stem);
        assert!(
            !file.vectors.is_empty(),
            "tests/vectors/{stem}.toml has no entries"
        );
        for v in &file.vectors {
            if !section_exists(&hs, v.section()) {
                problems.push(format!(
                    "{}: section {:?} is not a heading path of behaviors/{stem}.md",
                    v.origin,
                    v.section()
                ));
            }
            if v.kind() == DOC_ONLY && v.opt_str("note").is_none_or(str::is_empty) {
                problems.push(format!("{}: a {DOC_ONLY} entry needs a `note`", v.origin));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
