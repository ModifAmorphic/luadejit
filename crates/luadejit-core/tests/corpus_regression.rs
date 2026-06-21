//! Corpus regression test: decompiles real Darktide bytecode files.
//!
//! Run with: `cargo test --test corpus_regression -- --ignored`.
//!
//! Skips gracefully when the corpus is absent (CI doesn't have it).
//! The test always passes — it reports success/failure counts but
//! doesn't assert on individual file outcomes. The value is the
//! stderr summary: "X of 49 files decompile at Stage 10."

use std::fs;
use std::path::PathBuf;

/// Corpus root: the extracted Darktide tree. Resolved against the
/// home directory so the test works regardless of who runs it.
const CORPUS_ROOT: &str = "~/repos/ModifAmorphic/sandbox/extract-decompile/extracted";

/// Path (relative to the crate) of the v1 file subset list.
const V1_SUBSET: &str = "tests/corpus/v1.txt";

#[test]
#[ignore]
fn corpus_regression_v1() {
    let corpus_root = expand_tilde(CORPUS_ROOT);
    if !corpus_root.exists() {
        eprintln!(
            "skipping corpus regression: {} not found",
            corpus_root.display()
        );
        return;
    }

    let v1_paths = fs::read_to_string(V1_SUBSET).expect("v1.txt must exist");
    let files: Vec<&str> = v1_paths
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    eprintln!("running corpus regression on {} files", files.len());

    let mut success = 0;
    let mut not_implemented = 0;
    let mut invalid_bytecode = 0;
    let mut panics = 0;
    let mut missing = 0;

    for (i, rel_path) in files.iter().enumerate() {
        let full_path = corpus_root.join(rel_path);

        if !full_path.exists() {
            missing += 1;
            eprintln!("  [{}/{}] MISSING: {}", i + 1, files.len(), rel_path);
            continue;
        }

        let bytes = match fs::read(&full_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "  [{}/{}] READ ERROR: {} - {}",
                    i + 1,
                    files.len(),
                    rel_path,
                    e
                );
                missing += 1;
                continue;
            }
        };

        // Catch panics (crashes) — the decompiler shouldn't panic, but
        // real-world bytecode may surface edge cases. catch_unwind
        // keeps the run going so one bad file doesn't hide the rest.
        let result = std::panic::catch_unwind(|| luadejit_core::decompile(&bytes));

        match result {
            Ok(Ok(_source)) => {
                success += 1;
                eprintln!("  [{}/{}] SUCCESS: {}", i + 1, files.len(), rel_path);
            }
            Ok(Err(luadejit_core::DecompilerError::NotImplemented)) => {
                not_implemented += 1;
                // Not logged — most files land here at this stage and
                // the noise would drown out the interesting outcomes.
            }
            Ok(Err(luadejit_core::DecompilerError::InvalidBytecode { offset, reason })) => {
                invalid_bytecode += 1;
                eprintln!(
                    "  [{}/{}] INVALID: {} at offset {}: {}",
                    i + 1,
                    files.len(),
                    rel_path,
                    offset,
                    reason
                );
            }
            Err(_) => {
                panics += 1;
                eprintln!("  [{}/{}] PANIC: {}", i + 1, files.len(), rel_path);
            }
        }
    }

    eprintln!(
        "\ncorpus regression summary: {} files, {} success, {} NotImplemented, {} InvalidBytecode, {} panics, {} missing",
        files.len(),
        success,
        not_implemented,
        invalid_bytecode,
        panics,
        missing
    );
}

/// Expand a leading `~/` to the user's home directory. Paths without
/// a leading `~` are returned unchanged.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}
