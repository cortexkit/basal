use std::{collections::BTreeSet, fs, path::PathBuf};

use sha2::{Digest, Sha256};

#[test]
fn card_settled_sample_bytes_match_cingulate_checksums() {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/card-settled-v1");
    // Cingulate git commit 9866794 pins these independently of the local manifest.
    let source = [
        (
            "basal.basal.body.json",
            "a64c9ec9f426e6cbd6c6b3b64aa19cd78e7665730613a7f297841050b520c95d",
        ),
        (
            "basal.basal.headers.json",
            "86bce7df918386b93faa4e932b011348a44b10c1c72bd20dd14655c00825ae28",
        ),
        (
            "basal.basal.subject.txt",
            "8dbad73eda7c6f5653d1af9b815afeaea8b4a981a2177ee77f5ecbae1fb5342d",
        ),
    ];
    let manifest = fs::read_to_string(directory.join("SHA256SUMS")).unwrap();
    let mut covered = BTreeSet::new();
    for line in manifest.lines() {
        let (expected, name) = line.split_once("  ").unwrap();
        assert!(covered.insert(name), "duplicate checksum: {name}");
        assert_eq!(
            source.iter().find(|(file, _)| *file == name).unwrap().1,
            expected
        );
        let bytes = fs::read(directory.join(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), expected, "{name}");
    }
    assert_eq!(covered, source.iter().map(|(name, _)| *name).collect());
}
