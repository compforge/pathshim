use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) const HELP: &str = "\
Run a command with a best-effort root filesystem view.

Usage:
  pathshim -r <PATH> [-w <PATH>] [--] <COMMAND> [ARGS...]
  pathshim --rootfs=<PATH> [--cwd=<PATH>] [--] <COMMAND> [ARGS...]

Options:
  -r, --rootfs <PATH>  Directory presented as the command's best-effort root filesystem
  -w, --cwd, --pwd <PATH>
                       Initial working directory in the guest filesystem [default: /]
  -h, --help          Print help
";

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RunArgs {
    pub(crate) rootfs: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) command: OsString,
    pub(crate) args: Vec<OsString>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ParseResult {
    Run(RunArgs),
    Help,
}

pub(crate) fn parse<I>(args: I) -> Result<ParseResult, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let mut rootfs = None;
    let mut cwd = None;
    let mut command = Vec::new();
    let mut parsing_options = true;

    while let Some(arg) = args.next() {
        if parsing_options && arg == "--" {
            parsing_options = false;
            continue;
        }
        if parsing_options && (arg == "-h" || arg == "--help") {
            return Ok(ParseResult::Help);
        }
        if parsing_options && (arg == "-r" || arg == "--rootfs") {
            let Some(value) = args.next() else {
                return Err(format!("`{}` requires a path", arg.to_string_lossy()));
            };
            rootfs = Some(PathBuf::from(value));
            continue;
        }
        if parsing_options && (arg == "-w" || arg == "--cwd" || arg == "--pwd") {
            let Some(value) = args.next() else {
                return Err(format!("`{}` requires a path", arg.to_string_lossy()));
            };
            cwd = Some(PathBuf::from(value));
            continue;
        }
        if parsing_options {
            if let Some(value) = arg
                .to_str()
                .and_then(|value| value.strip_prefix("--rootfs="))
            {
                if value.is_empty() {
                    return Err("`--rootfs` requires a path".to_owned());
                }
                rootfs = Some(PathBuf::from(value));
                continue;
            }
            if let Some(value) = arg.to_str().and_then(|value| {
                ["--cwd=", "--pwd="]
                    .into_iter()
                    .find_map(|prefix| value.strip_prefix(prefix))
            }) {
                if value.is_empty() {
                    return Err("`--cwd` requires a path".to_owned());
                }
                cwd = Some(PathBuf::from(value));
                continue;
            }
            if arg.to_string_lossy().starts_with('-') {
                return Err(format!("unknown option `{}`", arg.to_string_lossy()));
            }
        }

        command.push(arg);
        command.extend(args);
        break;
    }

    let rootfs = rootfs.ok_or_else(|| "missing required option `--rootfs <PATH>`".to_owned())?;
    let mut command = command.into_iter();
    let executable = command.next().ok_or_else(|| "missing command".to_owned())?;

    Ok(ParseResult::Run(RunArgs {
        rootfs,
        cwd: cwd.unwrap_or_else(|| PathBuf::from("/")),
        command: executable,
        args: command.collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_short_rootfs_option() {
        let result = parse(os_args(&[
            "-r",
            "/rootfs/one",
            "--",
            "/bin/sh",
            "-c",
            "pwd",
        ]))
        .unwrap();

        assert_eq!(
            result,
            ParseResult::Run(RunArgs {
                rootfs: PathBuf::from("/rootfs/one"),
                cwd: PathBuf::from("/"),
                command: OsString::from("/bin/sh"),
                args: os_args(&["-c", "pwd"]),
            })
        );
    }

    #[test]
    fn accepts_command_without_separator() {
        let result =
            parse(os_args(&["--rootfs=/rootfs/one", "/bin/sh"])).expect("command should parse");

        assert!(matches!(result, ParseResult::Run(_)));
    }

    #[test]
    fn parses_guest_cwd_aliases() {
        for option in ["-w", "--cwd", "--pwd"] {
            let ParseResult::Run(args) = parse(os_args(&[
                "-r",
                "/rootfs/one",
                option,
                "/workspace",
                "true",
            ]))
            .unwrap() else {
                panic!("expected run arguments");
            };
            assert_eq!(args.cwd, PathBuf::from("/workspace"));
        }

        let ParseResult::Run(args) =
            parse(os_args(&["-r", "/rootfs/one", "--cwd=/workspace", "true"])).unwrap()
        else {
            panic!("expected run arguments");
        };
        assert_eq!(args.cwd, PathBuf::from("/workspace"));
    }

    #[test]
    fn rejects_missing_rootfs() {
        let error = parse(os_args(&["--", "/bin/sh"])).unwrap_err();

        assert!(error.contains("--rootfs"));
    }

    #[test]
    fn rejects_missing_command() {
        let error = parse(os_args(&["-r", "/rootfs/one"])).unwrap_err();

        assert!(error.contains("command"));
    }
}
