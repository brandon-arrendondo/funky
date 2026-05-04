/// Corpus integration tests.
///
/// For every file in tests/corpus/ the suite verifies:
///   1. `funky` exits 0 (no panic, no hard error).
///   2. Running `funky` a second time on its own output produces identical
///      output (idempotency).
///   3. If gcc or clang is available the formatted output compiles without
///      errors (compile-check is skipped silently when no compiler is found).
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Path to the compiled funky binary, set by Cargo at build time.
const FUNKY: &str = env!("CARGO_BIN_EXE_funky");

fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("tests/corpus directory missing")
        .filter_map(|e| {
            let e = e.ok()?;
            let p = e.path();
            let ext = p.extension()?.to_str()?;
            if matches!(ext, "c" | "cpp" | "h" | "hpp") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    files
}

/// Run `funky` on `input_path`, writing stdout to a temp file.
/// Returns the formatted text or panics with a diagnostic.
fn format_file(input_path: &Path, tmp_dir: &Path) -> String {
    let out = Command::new(FUNKY)
        .arg(input_path)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn funky: {e}"));

    assert!(
        out.status.success(),
        "funky exited non-zero for {}:\nstdout: {}\nstderr: {}",
        input_path.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let formatted = String::from_utf8(out.stdout).expect("funky output is not UTF-8");

    // Write the formatted output to a temp file for the idempotency pass.
    let stem = input_path.file_name().unwrap();
    let tmp = tmp_dir.join(stem);
    fs::write(&tmp, formatted.as_bytes()).expect("failed to write temp file");

    formatted
}

fn find_compiler(is_cpp: bool) -> Option<&'static str> {
    let candidates: &[&str] = if is_cpp {
        &["g++", "clang++"]
    } else {
        &["gcc", "clang"]
    };
    candidates.iter().copied().find(|cc| {
        Command::new(cc)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn check_compiles(source_path: &Path) {
    let ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let is_cpp = matches!(ext, "cpp" | "hpp");
    let Some(cc) = find_compiler(is_cpp) else {
        // No compiler present — skip gracefully.
        eprintln!(
            "skip compile check for {} (no compiler found)",
            source_path.display()
        );
        return;
    };

    let out = Command::new(cc)
        .args(["-fsyntax-only", "-Wall", "-Wno-unused"])
        .arg(source_path)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {cc}: {e}"));

    assert!(
        out.status.success(),
        "{cc} -fsyntax-only failed for {}:\n{}",
        source_path.display(),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn corpus_no_panic_idempotent_compiles() {
    let tmp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let pass1_dir = tmp_dir.path().join("pass1");
    let pass2_dir = tmp_dir.path().join("pass2");
    fs::create_dir_all(&pass1_dir).unwrap();
    fs::create_dir_all(&pass2_dir).unwrap();

    let files = corpus_files();
    assert!(!files.is_empty(), "tests/corpus/ contains no C/C++ files");

    for path in &files {
        eprintln!("testing: {}", path.display());

        // Pass 1 — format original.
        let pass1 = format_file(path, &pass1_dir);

        // Pass 2 — format the pass-1 output.
        let tmp1 = pass1_dir.join(path.file_name().unwrap());
        let pass2 = format_file(&tmp1, &pass2_dir);

        // Idempotency check.
        assert_eq!(
            pass1,
            pass2,
            "idempotency failure for {}:\n--- pass1 ---\n{}\n--- pass2 ---\n{}",
            path.display(),
            pass1,
            pass2,
        );

        // Compile check on pass-1 output.
        check_compiles(&tmp1);
    }
}
