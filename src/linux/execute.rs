use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::remote;
use super::seccomp::{self, SeccompNotif, SeccompNotifAddfd, SeccompNotifResp};
use super::State;

pub(crate) fn handle(listener: RawFd, state: &State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, flags) = if syscall == libc::SYS_execve {
        (libc::AT_FDCWD, notif.data.args[0], 0)
    } else {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[4] as i32,
        )
    };
    let raw_path = match remote::read_path(notif.pid, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if raw_path.as_os_str().is_empty() {
        // execveat(AT_EMPTY_PATH) already operates on a real fd. If that fd was
        // opened through a bind, the open handler injected the projected file.
        return respond_continue(listener, notif.id);
    }
    let virtual_path = match remote::resolve_path(state, notif.pid, dirfd, &raw_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if !state.view.projects(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
        // The internal /dev/fd path is itself a symlink, so continuing with this
        // flag would reject a non-symlink source and change execveat semantics.
        return respond_error(listener, notif.id, libc::ENOTSUP);
    }

    let real_path = match state.view.resolve_read(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let source_fd = match open_executable(&real_path) {
        Ok(fd) => fd,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let child_fd = match seccomp::add_fd(
        listener,
        &SeccompNotifAddfd {
            id: notif.id,
            flags: 0,
            srcfd: source_fd.as_raw_fd() as u32,
            newfd: 0,
            newfd_flags: libc::O_CLOEXEC as u32,
        },
    ) {
        Ok(fd) => fd,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let alias = executable_alias(child_fd);
    if let Err(error) = remote::rewrite_path(notif.pid, path_address, &raw_path, &alias) {
        return respond_error(listener, notif.id, errno(&error));
    }
    respond_continue(listener, notif.id)
}

fn open_executable(path: &Path) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn executable_alias(fd: i32) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{fd}"))
}

fn respond_continue(listener: RawFd, id: u64) -> io::Result<()> {
    seccomp::respond(
        listener,
        &SeccompNotifResp {
            id,
            val: 0,
            error: 0,
            flags: seccomp::USER_NOTIF_FLAG_CONTINUE,
        },
    )
}

fn respond_error(listener: RawFd, id: u64, error: i32) -> io::Result<()> {
    seccomp::respond(
        listener,
        &SeccompNotifResp {
            id,
            val: 0,
            error: -error,
            flags: 0,
        },
    )
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}
