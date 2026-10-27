#![cfg(test)]

use super::*;

#[test]
fn strategy_lib_collector_finds_dylibs_and_skips_other_files() {
    let dir = std::env::temp_dir().join(format!("bk-scan-test-{}", std::process::id()));
    let nested = dir.join("target/release");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(dir.join("liba.dylib"), b"").unwrap();
    std::fs::write(nested.join("libb.so"), b"").unwrap();
    std::fs::write(dir.join("readme.txt"), b"").unwrap();
    std::fs::write(dir.join("Cargo.toml"), b"").unwrap();
    let mut out = Vec::new();
    collect_strategy_libs(&dir, 0, &mut out);
    let mut names: Vec<String> = out
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["liba.dylib", "libb.so"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn strategy_lib_collector_respects_depth_bound() {
    let dir = std::env::temp_dir().join(format!("bk-deep-test-{}", std::process::id()));
    let mut deep = dir.clone();
    for i in 0..8 {
        deep = deep.join(format!("l{i}"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("toodeep.dylib"), b"").unwrap();
    let mut out = Vec::new();
    collect_strategy_libs(&dir, 0, &mut out);
    assert!(out.is_empty(), "beyond MAX_DEPTH must not be collected");
    std::fs::remove_dir_all(&dir).ok();
}
