use std::process::Command;

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
