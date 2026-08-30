#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestPaths {
    base: PathBuf,
    source: PathBuf,
}

impl TestPaths {
    fn new() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("pathshim-cwd-e2e-{}-{id}", std::process::id()));
        let source = base.join("workspace");
        fs::create_dir_all(&source).expect("create bind source fixture");
        Self { base, source }
    }
}

impl Drop for TestPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn initial_guest_cwd_sets_pwd_and_resolves_relative_writes() {
    let paths = TestPaths::new();
    let output = run_pathshim(
        &paths,
        "/workspace",
        "/bin/sh",
        &["-c", "printf '%s\\n' \"$PWD\"; pwd; echo cwd > result.txt"],
    );

    assert!(
        output.status.success(),
        "pathshim: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"/workspace\n/workspace\n");
    assert_eq!(
        fs::read_to_string(paths.source.join("result.txt")).unwrap(),
        "cwd\n"
    );
}

#[test]
fn missing_initial_guest_cwd_is_created_in_bind_source() {
    let paths = TestPaths::new();

    let output = run_pathshim(
        &paths,
        "/workspace/does-not-exist",
        "/bin/sh",
        &[
            "-c",
            "printf '%s\\n' \"$PWD\"; pwd; echo created > relative.txt",
        ],
    );

    assert!(
        output.status.success(),
        "pathshim: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"/workspace/does-not-exist\n/workspace/does-not-exist\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("created guest cwd path=/workspace/does-not-exist"));
    assert_eq!(
        fs::read_to_string(paths.source.join("does-not-exist/relative.txt")).unwrap(),
        "created\n"
    );
}

#[test]
fn non_directory_initial_guest_cwd_falls_back_to_root() {
    let paths = TestPaths::new();
    fs::write(paths.source.join("not-a-directory"), "file").unwrap();

    let output = run_pathshim(
        &paths,
        "/workspace/not-a-directory",
        "/bin/sh",
        &["-c", "printf '%s\\n' \"$PWD\"; pwd"],
    );

    assert!(
        output.status.success(),
        "pathshim: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"/\n/\n");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("guest cwd unavailable requested=/workspace/not-a-directory fallback=/"));
}

#[test]
fn cwd_namespace_matches_common_proot_process_semantics() {
    let paths = TestPaths::new();
    fs::create_dir_all(paths.source.join("start")).unwrap();
    let binary = paths.base.join("cwd-namespace");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cwd_namespace.c");
    let build = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile cwd fixture");
    assert!(
        build.status.success(),
        "cc: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let output = run_pathshim(&paths, "/workspace/start", binary.to_str().unwrap(), &[]);

    assert!(
        output.status.success(),
        "pathshim: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(paths.source.join("thread/relative-output")).unwrap(),
        "thread cwd\n"
    );
}

fn run_pathshim(
    paths: &TestPaths,
    cwd: &str,
    command: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("--bind")
        .arg(format!("{}:/workspace", paths.source.display()))
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
        .arg(command)
        .args(args)
        .output()
        .expect("run pathshim")
}
