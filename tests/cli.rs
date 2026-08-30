use std::process::Command;
#[cfg(not(target_os = "linux"))]
use std::{fs, path::PathBuf};

#[cfg(not(target_os = "linux"))]
struct TempDir(PathBuf);

#[cfg(not(target_os = "linux"))]
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pathshim-cli-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("create CLI test directory");
        Self(path)
    }
}

#[cfg(not(target_os = "linux"))]
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
