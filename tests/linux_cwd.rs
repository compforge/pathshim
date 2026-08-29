#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestPaths {
    base: PathBuf,
    rootfs: PathBuf,
}

impl TestPaths {
    fn new() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("pathshim-cwd-e2e-{}-{id}", std::process::id()));
        let rootfs = base.join("rootfs");
        fs::create_dir_all(&rootfs).expect("create rootfs fixture");
        Self { base, rootfs }
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
    fs::create_dir_all(paths.rootfs.join("workspace")).unwrap();

    let output = run_pathshim(
        &paths.rootfs,
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
        fs::read_to_string(paths.rootfs.join("workspace/result.txt")).unwrap(),
        "cwd\n"
    );
}

#[test]
fn missing_initial_guest_cwd_falls_back_to_root() {
    let paths = TestPaths::new();

    let output = run_pathshim(
        &paths.rootfs,
        "/does-not-exist",
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
        .contains("guest cwd unavailable requested=/does-not-exist fallback=/"));
}

#[test]
fn cwd_namespace_matches_common_proot_process_semantics() {
    let paths = TestPaths::new();
    fs::create_dir_all(paths.rootfs.join("start")).unwrap();
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

    let output = run_pathshim(&paths.rootfs, "/start", binary.to_str().unwrap(), &[]);

    assert!(
        output.status.success(),
        "pathshim: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(paths.rootfs.join("thread/relative-output")).unwrap(),
        "thread cwd\n"
    );
}

fn run_pathshim(rootfs: &Path, cwd: &str, command: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("--rootfs")
        .arg(rootfs)
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
        .arg(command)
        .args(args)
        .output()
        .expect("run pathshim")
}
