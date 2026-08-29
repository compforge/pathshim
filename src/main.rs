mod cli;
#[cfg(target_os = "linux")]
mod linux;
mod root;

use std::env;
use std::path::Path;
use std::process::{self, Command};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use cli::{ParseResult, HELP};
use root::{BindSpec, RootView};

// Startup contract: filesystem collection is optional. When a valid invocation
// discovers before exec that the requested view is unavailable, pathshim must
// degrade until it can run the original command; the terminal fallback preserves
// the caller's program, arguments, inherited environment, and working directory.
fn main() {
    let parsed = match cli::parse(env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("pathshim: {error}\n\n{HELP}");
            process::exit(2);
        }
    };

    let ParseResult::Run(args) = parsed else {
        print!("{HELP}");
        return;
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
    let mut root = match RootView::open_with_binds(args.rootfs.as_deref(), &binds) {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "pathshim: collect mode=passthrough reason=filesystem-view-unavailable rootfs={} binds={} error={error}",
                args.rootfs
                    .as_deref()
                    .map_or_else(|| "<none>".into(), |path| path.display().to_string()),
                binds.len()
            );
            exec(command);
        }
    };
    let guest_cwd = match root.resolve_directory(&args.cwd) {
        Ok(cwd) => cwd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match root.create_directory_all(&args.cwd) {
                Ok(cwd) => {
                    eprintln!("pathshim: created guest cwd path={}", cwd.display());
                    cwd
                }
                Err(error) => fallback_guest_cwd(&args.cwd, error),
            }
        }
        Err(error) => fallback_guest_cwd(&args.cwd, error),
    };
    configure_virtual_environment(&mut command, &guest_cwd);

    run(root, command, guest_cwd);
}

fn fallback_guest_cwd(requested: &Path, error: std::io::Error) -> std::path::PathBuf {
    eprintln!(
        "pathshim: guest cwd unavailable requested={} fallback=/ error={error}",
        requested.display()
    );
    Path::new("/").to_path_buf()
}

fn configure_virtual_environment(command: &mut Command, guest_cwd: &Path) {
    command
        .current_dir(guest_cwd)
        .env("PATHSHIM_ROOTFS", "/")
        .env("PWD", guest_cwd);
}

fn configure_fallback_environment(command: &mut Command, rootfs: &Path, cwd: &Path) {
    command
        .current_dir(cwd)
        .env("PATHSHIM_ROOTFS", rootfs)
        .env("PWD", cwd);
}

#[cfg(target_os = "linux")]
fn run(root: RootView, command: Command, guest_cwd: std::path::PathBuf) -> ! {
    match linux::run(root, command) {
        Ok(linux::RunOutcome::Exited(status)) => process::exit(status),
        Ok(linux::RunOutcome::Unavailable {
            root,
            command,
            reason,
        }) => {
            if let Some(rootfs) = root.root_upper() {
                let cwd = fallback_cwd(&root, &guest_cwd);
                let mut command = command;
                configure_fallback_environment(&mut command, rootfs, &cwd);
                eprintln!("pathshim: collect mode=cwd reason={reason}");
                exec(command);
            }
            eprintln!("pathshim: collect mode=passthrough reason={reason}");
            exec(passthrough_command(&command));
        }
        Err(error) => {
            eprintln!("pathshim: cannot run command with COW filesystem view: {error}");
            process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(root: RootView, command: Command, guest_cwd: std::path::PathBuf) -> ! {
    if let Some(rootfs) = root.root_upper() {
        let cwd = fallback_cwd(&root, &guest_cwd);
        let mut command = command;
        configure_fallback_environment(&mut command, rootfs, &cwd);
        eprintln!("pathshim: collect mode=cwd reason=cow-root-requires-linux");
        exec(command);
    }
    eprintln!("pathshim: collect mode=passthrough reason=cow-bind-requires-linux");
    exec(passthrough_command(&command));
}

fn passthrough_command(command: &Command) -> Command {
    // The COW attempt has already attached a virtual cwd and environment to
    // `command`. Rebuilding it is the only way to recover normal Command
    // inheritance because std::process::Command cannot clear current_dir.
    let mut passthrough = Command::new(command.get_program());
    passthrough.args(command.get_args());
    passthrough
}

fn fallback_cwd(root: &RootView, guest_cwd: &Path) -> std::path::PathBuf {
    match root.materialize_directory(guest_cwd) {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!(
                "pathshim: cannot materialize fallback cwd={} fallback={} error={error}",
                guest_cwd.display(),
                root.upper().display()
            );
            root.upper().to_path_buf()
        }
    }
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
    fn fallback_changes_only_root_marker_and_working_directory() {
        let rootfs = Path::new("/rootfs/one");
        let cwd = Path::new("/rootfs/one/workspace");
        let mut command = Command::new("true");
        command.env("HOME", "/caller/home");

        configure_fallback_environment(&mut command, rootfs, cwd);

        assert_eq!(command.get_current_dir(), Some(cwd));
        let environment: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            environment.get(OsStr::new("HOME")).copied().flatten(),
            Some(OsStr::new("/caller/home"))
        );
        assert_eq!(
            environment
                .get(OsStr::new("PATHSHIM_ROOTFS"))
                .copied()
                .flatten(),
            Some(OsStr::new("/rootfs/one"))
        );
        assert_eq!(
            environment.get(OsStr::new("PWD")).copied().flatten(),
            Some(OsStr::new("/rootfs/one/workspace"))
        );
    }

    #[test]
    fn terminal_passthrough_restores_normal_command_inheritance() {
        let mut configured = Command::new("/bin/sh");
        configured
            .args(["-c", "true"])
            .current_dir("/virtual/cwd")
            .env("PATHSHIM_ROOTFS", "/")
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
