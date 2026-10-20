use super::helpers::{repository_root, top_level_modules};
use std::fs;
#[test]
fn main_rs_does_not_duplicate_additional_library_modules() {
    let root = repository_root();
    let main = fs::read_to_string(root.join("src/main.rs")).expect("read src/main.rs");
    let library = fs::read_to_string(root.join("src/lib.rs")).expect("read src/lib.rs");
    let duplicates = top_level_modules(&main)
        .intersection(&top_level_modules(&library))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        duplicates.is_empty(),
        "src/main.rs must not redeclare library modules; found duplicates: {duplicates:?}"
    );
}
