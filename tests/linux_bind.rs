#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestPaths {
    base: PathBuf,
    source: PathBuf,
    destination: PathBuf,
}

impl TestPaths {
    fn new() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("pathshim-e2e-{}-{id}", std::process::id()));
        let source = base.join("source");
        let destination = base.join("guest");
        fs::create_dir_all(&source).expect("create bind source");
        fs::create_dir_all(&destination).expect("create guest destination fixture");
        Self {
            base,
            source,
            destination,
        }
    }
}

impl Drop for TestPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn bind_replaces_selected_subtree_and_leaves_outside_paths_alone() {
    let paths = TestPaths::new();
    fs::write(paths.source.join("input.txt"), "source\n").unwrap();
    fs::write(paths.destination.join("input.txt"), "hidden-destination\n").unwrap();
    fs::write(paths.destination.join("destination-only.txt"), "hidden\n").unwrap();
    let outside = paths.base.join("outside.txt");
    let destination = paths.destination.display();
    let script = format!(
        "cat {destination}/input.txt; \
         test ! -e {destination}/destination-only.txt; \
         echo mapped > {destination}/output.txt; \
         mkdir {destination}/dir; \
         echo nested > {destination}/dir/nested.txt; \
         mv {destination}/output.txt {destination}/renamed.txt; \
         echo passthrough > {}",
        outside.display()
    );

    let output = run_pathshim(&paths, "/bin/sh", &["-c", &script]);

    assert!(
        output.status.success(),
        "bind run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"source\n");
    assert_eq!(
        fs::read_to_string(paths.source.join("renamed.txt")).unwrap(),
        "mapped\n"
    );
    assert_eq!(
        fs::read_to_string(paths.source.join("dir/nested.txt")).unwrap(),
        "nested\n"
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "passthrough\n");
    assert_eq!(
        fs::read_to_string(paths.destination.join("input.txt")).unwrap(),
        "hidden-destination\n"
    );
    assert!(!paths.source.join(".pathshim").exists());
}

#[test]
fn concurrent_descendants_share_one_bind_view() {
    let paths = TestPaths::new();
    let destination = paths.destination.display();
    let script = format!(
        "/bin/sh -c 'echo first > {destination}/first.txt' & \
         /bin/sh -c 'echo second > {destination}/second.txt' & \
         wait; cat {destination}/first.txt {destination}/second.txt"
    );

    let output = run_pathshim(&paths, "/bin/sh", &["-c", &script]);

    assert!(output.status.success());
    assert_eq!(output.stdout, b"first\nsecond\n");
}

#[test]
fn independent_invocations_share_one_source_while_both_are_active() {
    let paths = TestPaths::new();
    let destination = paths.destination.display();
    let first_script = format!(
        "echo first > {destination}/first.txt; \
         while test ! -e {destination}/release; do sleep 0.01; done; \
         cat {destination}/second.txt"
    );
    let first = spawn_pathshim(&paths, "/bin/sh", &["-c", &first_script]);
    wait_until_exists(&paths.source.join("first.txt"));

    let second_script = format!(
        "cat {destination}/first.txt; \
         echo second > {destination}/second.txt; \
         echo release > {destination}/release"
    );
    let second = run_pathshim(&paths, "/bin/sh", &["-c", &second_script]);
    let first = first.wait_with_output().expect("wait for first invocation");

    assert!(first.status.success());
    assert!(second.status.success());
    assert_eq!(second.stdout, b"first\n");
    assert_eq!(first.stdout, b"second\n");
}

#[test]
fn active_invocation_observes_an_external_writer() {
    let paths = TestPaths::new();
    let destination = paths.destination.display();
    let script = format!(
        "echo ready > {destination}/ready; \
         while test ! -e {destination}/uploaded.txt; do sleep 0.01; done; \
         cat {destination}/uploaded.txt"
    );
    let child = spawn_pathshim(&paths, "/bin/sh", &["-c", &script]);
    wait_until_exists(&paths.source.join("ready"));

    fs::write(paths.source.join("uploaded.txt"), "external\n").unwrap();
    let output = child.wait_with_output().expect("wait for pathshim");

    assert!(
        output.status.success(),
        "external writer run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"external\n");
}

#[test]
fn static_go_program_uses_the_bind_view() {
    if Command::new("go").arg("version").output().is_err() {
        eprintln!("skipping static Go coverage: go is not installed");
        return;
    }

    let paths = TestPaths::new();
    let binary = paths.base.join("static-go");
    build_static_go(&binary);

    let output = Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("--bind")
        .arg(format!("{}:/project", paths.source.display()))
        .arg("--")
        .arg(&binary)
        .output()
        .expect("run static Go fixture");

    assert!(
        output.status.success(),
        "static Go bind run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(paths.source.join("go-output")).unwrap(),
        output.stdout
    );
}

#[test]
fn initial_command_executes_elf_from_bind_source() {
    if Command::new("go").arg("version").output().is_err() {
        eprintln!("skipping mapped executable coverage: go is not installed");
        return;
    }

    let paths = TestPaths::new();
    let binary = paths.source.join("app");
    build_static_go(&binary);
    let executable = paths.destination.join("app");
    let output_path = paths.destination.join("initial-exec-output");

    let output = run_pathshim(
        &paths,
        executable.to_str().unwrap(),
        &[output_path.to_str().unwrap()],
    );

    assert!(
        output.status.success(),
        "mapped initial exec: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(paths.source.join("initial-exec-output")).unwrap(),
        output.stdout
    );
}

#[test]
fn mapped_exec_does_not_fall_back_to_destination() {
    let paths = TestPaths::new();
    let executable = paths.destination.join("destination-only-app");
    fs::copy("/bin/true", &executable).expect("create destination executable fixture");

    let output = run_pathshim(&paths, executable.to_str().unwrap(), &[]);

    assert!(!output.status.success());
    assert!(!paths.source.join("destination-only-app").exists());
}

#[test]
fn command_builds_and_executes_elf_through_bind() {
    if Command::new("go").arg("version").output().is_err() {
        eprintln!("skipping build-and-exec coverage: go is not installed");
        return;
    }

    let paths = TestPaths::new();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/static_go.go");
    let executable = paths.destination.join("built-app");
    let output_path = paths.destination.join("built-exec-output");
    let script = format!(
        "CGO_ENABLED=0 go build -o {executable} {fixture} && {executable} {output_path}",
        executable = executable.display(),
        fixture = fixture.display(),
        output_path = output_path.display(),
    );

    let output = run_pathshim(&paths, "/bin/sh", &["-c", &script]);

    assert!(
        output.status.success(),
        "mapped descendant exec: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(paths.source.join("built-exec-output")).unwrap(),
        output.stdout
    );
}

#[test]
fn execveat_executes_elf_from_bind_source() {
    if Command::new("go").arg("version").output().is_err() {
        eprintln!("skipping execveat coverage: go is not installed");
        return;
    }

    let paths = TestPaths::new();
    build_static_go(&paths.source.join("app"));
    let launcher = paths.base.join("execveat-launcher");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/execveat.c");
    let build = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(fixture)
        .arg("-o")
        .arg(&launcher)
        .output()
        .expect("compile execveat fixture");
    assert!(
        build.status.success(),
        "cc: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let output = Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("--bind")
        .arg(format!("{}:/workspace", paths.source.display()))
        .arg("--")
        .arg(launcher)
        .output()
        .expect("run execveat fixture");

    assert!(
        output.status.success(),
        "mapped execveat: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(paths.source.join("execveat-output")).unwrap(),
        output.stdout
    );
}

#[test]
fn python_program_and_child_share_the_bind_view() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping Python coverage: python3 is not installed");
        return;
    }

    let paths = TestPaths::new();
    fs::write(paths.source.join("input.txt"), "python-source\n").unwrap();
    let code = r#"
from pathlib import Path
import subprocess
import sys

destination = Path(sys.argv[1])
print((destination / "input.txt").read_text().strip())
(destination / "parent.txt").write_text("parent")
subprocess.run(
    [sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('child')", str(destination / "child.txt")],
    check=True,
)
"#;

    let output = run_pathshim(
        &paths,
        "python3",
        &["-c", code, paths.destination.to_str().unwrap()],
    );

    assert!(
        output.status.success(),
        "Python bind run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"python-source\n");
    assert_eq!(
        fs::read_to_string(paths.source.join("parent.txt")).unwrap(),
        "parent"
    );
    assert_eq!(
        fs::read_to_string(paths.source.join("child.txt")).unwrap(),
        "child"
    );
}

#[test]
fn inherits_callers_process_group_and_forwards_termination_signal() {
    let paths = TestPaths::new();
    let mut child = command(&paths)
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

#[test]
fn preserves_signal_terminal_status() {
    let paths = TestPaths::new();
    let output = run_pathshim(&paths, "/bin/sh", &["-c", "kill -TERM $$"]);

    assert_eq!(output.status.signal(), Some(libc::SIGTERM));
    assert_eq!(output.status.code(), None);
}

#[test]
fn quiet_mode_keeps_pathshim_diagnostics_out_of_stderr() {
    let paths = TestPaths::new();
    let output = command(&paths)
        .args(["--quiet", "--", "/bin/true"])
        .output()
        .expect("run quiet pathshim");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}

fn command(paths: &TestPaths) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pathshim"));
    command.arg("--bind").arg(format!(
        "{}:{}",
        paths.source.display(),
        paths.destination.display()
    ));
    command
}

fn run_pathshim(paths: &TestPaths, executable: &str, args: &[&str]) -> std::process::Output {
    command(paths)
        .arg("--")
        .arg(executable)
        .args(args)
        .output()
        .expect("run pathshim")
}

fn spawn_pathshim(paths: &TestPaths, executable: &str, args: &[&str]) -> Child {
    command(paths)
        .arg("--")
        .arg(executable)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pathshim")
}

fn build_static_go(binary: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/static_go.go");
    let build = Command::new("go")
        .env("CGO_ENABLED", "0")
        .args(["build", "-o"])
        .arg(binary)
        .arg(source)
        .output()
        .expect("build static Go fixture");
    assert!(
        build.status.success(),
        "go build: {}",
        String::from_utf8_lossy(&build.stderr)
    );
}

fn wait_until_exists(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
