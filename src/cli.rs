use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) const HELP: &str = "\
Run a command with a best-effort root filesystem view.

Usage:
  pathshim -r <PATH> [-b <SOURCE:DEST>] [-w <PATH>] [--] <COMMAND> [ARGS...]
  pathshim --bind=<SOURCE:DEST> [--cwd=<PATH>] [--] <COMMAND> [ARGS...]

Options:
  -r, --rootfs <PATH>  Directory presented as the command's best-effort root filesystem
  -b, --bind <SOURCE:DEST>
                       Collect writes below guest DEST into SOURCE; may be repeated
  -w, --cwd, --pwd <PATH>
                       Initial working directory in the guest filesystem [default: /]
  -h, --help          Print help
";

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RunArgs {
    pub(crate) rootfs: Option<PathBuf>,
    pub(crate) binds: Vec<BindArg>,
    pub(crate) cwd: PathBuf,
    pub(crate) command: OsString,
    pub(crate) args: Vec<OsString>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct BindArg {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
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
    let mut binds = Vec::new();
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
        if parsing_options && (arg == "-b" || arg == "--bind") {
            let Some(value) = args.next() else {
                return Err(format!("`{}` requires SOURCE:DEST", arg.to_string_lossy()));
            };
            binds.push(parse_bind(value)?);
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
            if let Some(value) = arg.to_str().and_then(|value| value.strip_prefix("--bind=")) {
                binds.push(parse_bind(OsString::from(value))?);
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

    if rootfs.is_none() && binds.is_empty() {
        return Err(
            "missing filesystem view: provide `--rootfs <PATH>` or `--bind SOURCE:DEST`".to_owned(),
        );
    }
    let mut command = command.into_iter();
    let executable = command.next().ok_or_else(|| "missing command".to_owned())?;

    Ok(ParseResult::Run(RunArgs {
        rootfs,
        binds,
        cwd: cwd.unwrap_or_else(|| PathBuf::from("/")),
        command: executable,
        args: command.collect(),
    }))
}

fn parse_bind(value: OsString) -> Result<BindArg, String> {
    let value = value
        .to_str()
        .ok_or_else(|| "`--bind` requires UTF-8 SOURCE:DEST paths".to_owned())?;
    let Some((source, destination)) = value.split_once(':') else {
        return Err(format!("invalid bind `{value}`: expected SOURCE:DEST"));
    };
    if source.is_empty() || destination.is_empty() {
        return Err(format!(
            "invalid bind `{value}`: SOURCE and DEST must not be empty"
        ));
    }
    let destination = PathBuf::from(destination);
    if !destination.is_absolute() {
        return Err(format!(
            "invalid bind `{value}`: DEST must be an absolute guest path"
        ));
    }
    Ok(BindArg {
        source: PathBuf::from(source),
        destination,
    })
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
                rootfs: Some(PathBuf::from("/rootfs/one")),
                binds: Vec::new(),
                cwd: PathBuf::from("/"),
                command: OsString::from("/bin/sh"),
                args: os_args(&["-c", "pwd"]),
            })
        );
    }

    #[test]
    fn parses_repeatable_binds_without_rootfs() {
        let result = parse(os_args(&[
            "--bind",
            "/upper/one:/guest/one",
            "--bind=/upper/two:/guest/two",
            "true",
        ]))
        .unwrap();

        assert_eq!(
            result,
            ParseResult::Run(RunArgs {
                rootfs: None,
                binds: vec![
                    BindArg {
                        source: PathBuf::from("/upper/one"),
                        destination: PathBuf::from("/guest/one"),
                    },
                    BindArg {
                        source: PathBuf::from("/upper/two"),
                        destination: PathBuf::from("/guest/two"),
                    },
                ],
                cwd: PathBuf::from("/"),
                command: OsString::from("true"),
                args: Vec::new(),
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
    fn rejects_missing_filesystem_view() {
        let error = parse(os_args(&["--", "/bin/sh"])).unwrap_err();

        assert!(error.contains("--rootfs"));
        assert!(error.contains("--bind"));
    }

    #[test]
    fn rejects_invalid_bind_destination() {
        let error = parse(os_args(&["--bind", "/upper:relative", "true"])).unwrap_err();

        assert!(error.contains("absolute guest path"));
    }

    #[test]
    fn rejects_missing_command() {
        let error = parse(os_args(&["-r", "/rootfs/one"])).unwrap_err();

        assert!(error.contains("command"));
    }
}
