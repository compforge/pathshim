#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestPaths {
    base: PathBuf,
    rootfs: PathBuf,
    lower: PathBuf,
}

impl TestPaths {
    fn new() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("pathshim-e2e-{}-{id}", std::process::id()));
        let rootfs = base.join("rootfs");
        let lower = base.join("merged");
        fs::create_dir_all(&lower).expect("create lower fixture");
        Self {
            base,
            rootfs,
            lower,
        }
    }

    fn virtual_lower(&self) -> &Path {
        &self.lower
    }

    fn upper_for(&self, virtual_path: &Path) -> PathBuf {
        self.rootfs.join(virtual_path.strip_prefix("/").unwrap())
    }
}

impl Drop for TestPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn cow_view_merges_reads_collects_writes_and_persists_whiteouts() {
    let paths = TestPaths::new();
    fs::write(paths.lower.join("lower.txt"), "lower\n").unwrap();
    fs::write(paths.lower.join("metadata.txt"), "metadata\n").unwrap();
    let virtual_dir = paths.virtual_lower().display();
    let script = format!(
        "cat {virtual_dir}/lower.txt; \
         echo upper > {virtual_dir}/upper.txt; \
         mkdir /project; \
         cd /project; \
         test \"$(pwd)\" = /project; \
         echo relative > relative.txt; \
         chmod 600 {virtual_dir}/metadata.txt; \
         ls {virtual_dir} | sort; \
         rm {virtual_dir}/lower.txt"
    );

    let first = run_pathshim(&paths.rootfs, "/bin/sh", &["-c", &script]);
    assert!(
        first.status.success(),
        "first run: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("lower\n"));
    assert!(stdout.contains("lower.txt\nmetadata.txt\nupper.txt\n"));
    assert_eq!(
        fs::read_to_string(paths.upper_for(&paths.lower.join("upper.txt"))).unwrap(),
        "upper\n"
    );
    assert_eq!(
        fs::read_to_string(paths.rootfs.join("project/relative.txt")).unwrap(),
        "relative\n"
    );
    assert_eq!(
        fs::read_to_string(paths.lower.join("lower.txt")).unwrap(),
        "lower\n"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(paths.upper_for(&paths.lower.join("metadata.txt")))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let second_script = format!("test ! -e {virtual_dir}/lower.txt && cat {virtual_dir}/upper.txt");
    let second = run_pathshim(&paths.rootfs, "/bin/sh", &["-c", &second_script]);
    assert!(
        second.status.success(),
        "second run: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(second.stdout, b"upper\n");
}

#[test]
fn static_go_program_uses_the_same_cow_view() {
    if Command::new("go").arg("version").output().is_err() {
        eprintln!("skipping static Go coverage: go is not installed");
        return;
    }

    let paths = TestPaths::new();
    let binary = paths.base.join("static-go");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/static_go.go");
    let build = Command::new("go")
        .env("CGO_ENABLED", "0")
        .args(["build", "-o"])
        .arg(&binary)
        .arg(source)
        .output()
        .expect("build static Go fixture");
    assert!(
        build.status.success(),
        "go build: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let output = run_pathshim(&paths.rootfs, binary.to_str().unwrap(), &[]);
    assert!(
        output.status.success(),
        "static Go run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(paths.rootfs.join("project/go-output")).unwrap(),
        output.stdout
    );
    assert!(!output.stdout.is_empty());
}

#[test]
fn inherits_callers_process_group_and_forwards_termination_signal() {
    let paths = TestPaths::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("--rootfs")
        .arg(&paths.rootfs)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "trap 'exit 42' TERM; echo $$; while :; do sleep 1; done",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("start pathshim");
    let stdout = child.stdout.take().expect("capture stdout");
    let mut command_pid = String::new();
    BufReader::new(stdout)
        .read_line(&mut command_pid)
        .expect("read command pid");
    let command_pid = command_pid
        .trim()
        .parse::<i32>()
        .expect("parse command pid");
    let pathshim_pgid = unsafe { libc::getpgid(child.id() as i32) };
    let command_pgid = unsafe { libc::getpgid(command_pid) };
    assert!(pathshim_pgid > 0, "get pathshim pgid");
    assert_eq!(command_pgid, pathshim_pgid);

    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let status = child.wait().expect("wait for pathshim");
    assert_eq!(status.code(), Some(42));
}

fn run_pathshim(rootfs: &Path, command: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("--rootfs")
        .arg(rootfs)
        .arg("--")
        .arg(command)
        .args(args)
        .output()
        .expect("run pathshim")
}
