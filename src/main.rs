mod bind;
mod cli;
#[cfg(target_os = "linux")]
mod linux;

use std::env;
use std::path::Path;
use std::process::{self, Command};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use bind::{BindSpec, BindView};
use cli::{ParseResult, HELP};

const VERSION: &str = include_str!("../VERSION");

// Startup contract: path mapping is optional. When a valid invocation cannot
// install its bind view before exec, the original command still starts with its
// inherited environment and caller working directory.
fn main() {
    let parsed = match cli::parse(env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("pathshim: {error}\n\n{HELP}");
            process::exit(2);
        }
    };

    let args = match parsed {
        ParseResult::Run(args) => args,
        ParseResult::Help => {
            print!("{HELP}");
            return;
        }
        ParseResult::Version => {
            println!("pathshim {}", VERSION.trim());
            return;
        }
    };

    let mut command = Command::new(&args.command);
    command.args(&args.args);
    let binds: Vec<_> = args
        .binds
        .into_iter()
        .map(|bind| BindSpec {
            source: bind.source,
            destination: bind.destination,
        })
        .collect();
    let view = match BindView::open(&binds) {
        Ok(view) => view,
        Err(error) => {
            diagnostic(
                args.quiet,
                format!(
                    "collect mode=passthrough reason=bind-view-unavailable binds={} error={error}",
                    binds.len()
                ),
            );
            exec(command);
        }
    };
    let guest_cwd = match view.resolve_directory(&args.cwd) {
        Ok(cwd) => cwd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match view.create_directory_all(&args.cwd) {
                Ok(cwd) => {
                    diagnostic(
                        args.quiet,
                        format!("created guest cwd path={}", cwd.display()),
                    );
                    cwd
                }
                Err(error) => fallback_guest_cwd(args.quiet, &args.cwd, error),
            }
        }
        Err(error) => fallback_guest_cwd(args.quiet, &args.cwd, error),
    };
    configure_virtual_environment(&mut command, &guest_cwd);

    run(view, command, args.quiet);
}

fn diagnostic(quiet: bool, message: String) {
    if !quiet {
        eprintln!("pathshim: {message}");
    }
}

fn fallback_guest_cwd(quiet: bool, requested: &Path, error: std::io::Error) -> std::path::PathBuf {
    diagnostic(
        quiet,
        format!(
            "guest cwd unavailable requested={} fallback=/ error={error}",
            requested.display()
        ),
    );
    Path::new("/").to_path_buf()
}

fn configure_virtual_environment(command: &mut Command, guest_cwd: &Path) {
    command.current_dir(guest_cwd).env("PWD", guest_cwd);
}

#[cfg(target_os = "linux")]
fn run(view: BindView, command: Command, quiet: bool) -> ! {
    match linux::run(view, command, quiet) {
        Ok(linux::RunOutcome::Exited(status)) => linux::exit_with_status(status),
        Ok(linux::RunOutcome::Unavailable { command, reason }) => {
            diagnostic(quiet, format!("collect mode=passthrough reason={reason}"));
            exec(passthrough_command(&command));
        }
        Err(error) => {
            eprintln!("pathshim: bind view failed after command startup: {error}");
            process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_view: BindView, command: Command, quiet: bool) -> ! {
    diagnostic(
        quiet,
        "collect mode=passthrough reason=bind-view-requires-linux".to_owned(),
    );
    exec(passthrough_command(&command));
}

fn passthrough_command(command: &Command) -> Command {
    // The bind attempt attached a guest cwd and PWD. Rebuilding is the only
    // way to recover normal Command inheritance because Command cannot clear
    // current_dir once configured.
    let mut passthrough = Command::new(command.get_program());
    passthrough.args(command.get_args());
    passthrough
}

#[cfg(unix)]
fn exec(mut command: Command) -> ! {
    let error = command.exec();
    eprintln!(
        "pathshim: cannot execute command `{}`: {error}",
        command.get_program().to_string_lossy()
    );
    process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn terminal_passthrough_restores_normal_command_inheritance() {
        let mut configured = Command::new("/bin/sh");
        configured
            .args(["-c", "true"])
            .current_dir("/virtual/cwd")
            .env("PWD", "/virtual/cwd");

        let passthrough = passthrough_command(&configured);

        assert_eq!(passthrough.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(
            passthrough.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("-c"), OsStr::new("true")]
        );
        assert_eq!(passthrough.get_current_dir(), None);
        assert_eq!(passthrough.get_envs().count(), 0);
    }
}
