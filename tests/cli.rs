use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fs,
    path::{Path, PathBuf},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pathshim-cli-test-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("create CLI test directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn version_options_print_the_package_version() {
    let version = include_str!("../VERSION").trim();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));

    for option in ["-V", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pathshim"))
            .arg(option)
            .output()
            .expect("pathshim should start");

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("pathshim {version}\n")
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn invalid_probe_invocation_exits_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("probe")
        .output()
        .expect("pathshim probe should start");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("probe requires at least one"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn probe_reports_passthrough_off_linux() {
    let temp = TempDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_pathshim"))
        .arg("probe")
        .arg("--bind")
        .arg(format!("{}:/workspace", temp.0.display()))
        .output()
        .expect("pathshim probe should start");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"passthrough\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("bind-view-requires-linux"));
}

fn invocation(source: &Path, cwd: &str, probe: bool, marker: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pathshim"));
    if probe {
        command.arg("probe");
    } else {
        command.arg("--quiet");
    }
    command
        .arg("--bind")
        .arg(format!("{}:/workspace", source.display()))
        .arg("--cwd")
        .arg(cwd);
    if !probe {
        command
            .args(["--", "/bin/sh", "-c", "printf ran > \"$1\"", "fixture"])
            .arg(marker);
    }
    command
}

// +case:id=invalid_mapping_never_runs,desc=`Supply invalid source or duplicate destination under quiet mode and probe`,expect=`Exit 2 with diagnostics and no command or source mutation`
#[test]
fn invalid_sources_and_duplicate_destinations_never_run_or_create_sources() {
    for probe in [false, true] {
        let temp = TempDir::new();
        let marker = temp.0.join("ran");
        let missing = temp.0.join("missing");
        let file = temp.0.join("file");
        fs::write(&file, "not a directory").unwrap();
        for source in [&missing, &file] {
            let output = invocation(source, "/workspace", probe, &marker)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            assert!(output.stdout.is_empty());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains("stage=source")
                    && error.contains("destination=\"/workspace\"")
                    && error.contains("errno="),
                "{error}"
            );
            assert!(!marker.exists());
            assert!(!missing.exists());
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_pathshim"));
        if probe {
            command.arg("probe");
        } else {
            command.arg("--quiet");
        }
        command
            .arg("--bind")
            .arg(format!("{}:/workspace", temp.0.display()))
            .arg("--bind")
            .arg(format!("{}:/workspace/./", temp.0.display()));
        if !probe {
            command
                .args(["--", "/bin/sh", "-c", "printf ran > \"$1\"", "fixture"])
                .arg(&marker);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate bind destination"));
        assert!(!marker.exists());
    }
}

#[test]
fn invalid_cwd_never_creates_or_falls_back_to_another_directory() {
    for probe in [false, true] {
        let temp = TempDir::new();
        let marker = temp.0.join("ran");
        fs::write(temp.0.join("file"), "file").unwrap();
        for cwd in ["/workspace/missing", "/workspace/file"] {
            let output = invocation(&temp.0, cwd, probe, &marker).output().unwrap();
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("stage=cwd"));
            assert!(!marker.exists());
            assert!(!temp.0.join("missing").exists());
        }
    }
}

// +case:id=invocation_identity_permissions,desc=`An existing source has an unsearchable ancestor or cwd`,expect=`The actual invocation identity gets EACCES without chmod or passthrough`
#[cfg(unix)]
#[test]
fn inaccessible_source_ancestor_and_cwd_remain_inaccessible() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new();
    let parent = temp.0.join("private");
    let source = parent.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::metadata(&source).is_ok() {
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("SKIP: invocation can bypass directory permissions; run as an unprivileged UID");
        return;
    }
    let marker = temp.0.join("ran");
    let attempts: Vec<_> = [false, true]
        .into_iter()
        .map(|probe| {
            invocation(&source, "/workspace", probe, &marker)
                .output()
                .unwrap()
        })
        .collect();
    let mode = fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(mode, 0);
    for output in attempts {
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("stage=source"));
        assert!(!marker.exists());
    }
    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
    let attempts: Vec<_> = [false, true]
        .into_iter()
        .map(|probe| {
            invocation(&temp.0, "/workspace/private/source", probe, &marker)
                .output()
                .unwrap()
        })
        .collect();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o777)).unwrap();
    for output in attempts {
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("stage=cwd"));
        assert!(!marker.exists());
    }
}

#[test]
fn every_source_is_validated_before_any_command_or_probe() {
    let temp = TempDir::new();
    let marker = temp.0.join("ran");
    let missing = temp.0.join("missing");
    for probe in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pathshim"));
        if probe {
            command.arg("probe");
        } else {
            command.arg("--quiet");
        }
        command
            .arg("--bind")
            .arg(format!("{}:/one/deep", temp.0.display()))
            .arg("--bind")
            .arg(format!("{}:/two", missing.display()));
        if !probe {
            command
                .args(["--", "/bin/sh", "-c", "printf ran > \"$1\"", "fixture"])
                .arg(&marker);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("destination=\"/two\""));
        assert!(!missing.exists());
        assert!(!marker.exists());
    }
}
