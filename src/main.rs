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
use root::RootView;

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

    let root = match RootView::open(&args.rootfs) {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "pathshim: collect mode=passthrough reason=rootfs-unavailable rootfs={} error={error}",
                args.rootfs.display()
            );
            exec(command);
        }
    };
    configure_virtual_environment(&mut command);

    run(root, command);
}

fn configure_virtual_environment(command: &mut Command) {
    command
        .current_dir("/")
        .env("PATHSHIM_ROOTFS", "/")
        .env("PWD", "/");
}

fn configure_fallback_environment(command: &mut Command, rootfs: &Path) {
    command
        .current_dir(rootfs)
        .env("PATHSHIM_ROOTFS", rootfs)
        .env("PWD", rootfs);
}

#[cfg(target_os = "linux")]
fn run(root: RootView, command: Command) -> ! {
    match linux::run(root, command) {
        Ok(linux::RunOutcome::Exited(status)) => process::exit(status),
        Ok(linux::RunOutcome::Unavailable {
            root,
            mut command,
            reason,
        }) => {
            configure_fallback_environment(&mut command, root.upper());
            eprintln!("pathshim: collect mode=cwd reason={reason}");
            exec(command);
        }
        Err(error) => {
            eprintln!("pathshim: cannot run command with COW root: {error}");
            process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(root: RootView, mut command: Command) -> ! {
    configure_fallback_environment(&mut command, root.upper());
    eprintln!("pathshim: collect mode=cwd reason=cow-root-requires-linux");
    exec(command);
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
        let mut command = Command::new("true");
        command.env("HOME", "/caller/home");

        configure_fallback_environment(&mut command, rootfs);

        assert_eq!(command.get_current_dir(), Some(rootfs));
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
    }
}
