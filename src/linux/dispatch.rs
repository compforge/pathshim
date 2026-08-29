use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::remote;
use super::seccomp::{self, SeccompNotif, SeccompNotifAddfd, SeccompNotifResp};
use super::sysno;
use super::{OpenDirectory, State};

#[repr(C)]
#[derive(Clone, Copy)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

pub(crate) fn handle(listener: RawFd, state: &mut State, notif: SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    if syscall == sysno::OPEN || syscall == libc::SYS_openat || syscall == libc::SYS_openat2 {
        return handle_open(listener, state, &notif);
    }
    if syscall == libc::SYS_newfstatat {
        return handle_stat(listener, state, &notif);
    }
    if syscall == libc::SYS_statx {
        return handle_statx(listener, state, &notif);
    }
    if syscall == sysno::ACCESS || syscall == libc::SYS_faccessat || syscall == libc::SYS_faccessat2
    {
        return handle_access(listener, state, &notif);
    }
    if syscall == sysno::MKDIR || syscall == libc::SYS_mkdirat {
        return handle_mkdir(listener, state, &notif);
    }
    if syscall == sysno::UNLINK || syscall == libc::SYS_unlinkat || syscall == sysno::RMDIR {
        return handle_unlink(listener, state, &notif);
    }
    if syscall == sysno::RENAME || syscall == libc::SYS_renameat || syscall == libc::SYS_renameat2 {
        return handle_rename(listener, state, &notif);
    }
    if syscall == sysno::READLINK || syscall == libc::SYS_readlinkat {
        return handle_readlink(listener, state, &notif);
    }
    if syscall == sysno::SYMLINK || syscall == libc::SYS_symlinkat {
        return handle_symlink(listener, state, &notif);
    }
    if syscall == libc::SYS_getdents64 {
        return handle_getdents(listener, state, &notif);
    }
    if syscall == libc::SYS_chdir || syscall == libc::SYS_fchdir {
        return handle_chdir(listener, state, &notif);
    }
    if syscall == libc::SYS_getcwd {
        return handle_getcwd(listener, state, &notif);
    }
    if syscall == libc::SYS_truncate {
        return handle_truncate(listener, state, &notif);
    }
    if syscall == sysno::CHMOD || syscall == libc::SYS_fchmodat {
        return handle_chmod(listener, state, &notif);
    }
    if syscall == sysno::CHOWN || syscall == sysno::LCHOWN || syscall == libc::SYS_fchownat {
        return handle_chown(listener, state, &notif);
    }
    if syscall == libc::SYS_utimensat {
        return handle_utimensat(listener, state, &notif);
    }
    respond_continue(listener, notif.id)
}

fn handle_open(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, flags, mode) = if syscall == sysno::OPEN {
        (
            libc::AT_FDCWD,
            notif.data.args[0],
            notif.data.args[1] as i32,
            notif.data.args[2] as u32,
        )
    } else if syscall == libc::SYS_openat {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2] as i32,
            notif.data.args[3] as u32,
        )
    } else {
        let how: OpenHow = match remote::read_value(notif.pid, notif.data.args[2]) {
            Ok(how) => how,
            Err(error) => return respond_error(listener, notif.id, errno(&error)),
        };
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            how.flags as i32,
            how.mode as u32,
        )
    };
    let virtual_path = match read_and_resolve(state, notif, dirfd, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.prepare_open(&virtual_path, flags) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = match CString::new(real_path.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(_) => return respond_error(listener, notif.id, libc::EINVAL),
    };
    let parent_flags = flags & !libc::O_CLOEXEC;
    let source_fd = unsafe { libc::open(path.as_ptr(), parent_flags, mode) };
    if source_fd < 0 {
        return respond_error(listener, notif.id, errno(&io::Error::last_os_error()));
    }
    let source_fd = unsafe { OwnedFd::from_raw_fd(source_fd) };
    let addfd = SeccompNotifAddfd {
        id: notif.id,
        flags: seccomp::ADDFD_FLAG_SEND,
        srcfd: source_fd.as_raw_fd() as u32,
        newfd: 0,
        newfd_flags: (flags & libc::O_CLOEXEC) as u32,
    };
    let child_fd = match seccomp::add_fd(listener, &addfd) {
        Ok(fd) => fd,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };

    if flags & libc::O_DIRECTORY != 0
        || real_path
            .metadata()
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
    {
        if let Ok(entries) = state.root.list_dir(&virtual_path) {
            state.directories.insert(
                (notif.pid, child_fd),
                OpenDirectory {
                    path: virtual_path,
                    entries,
                    cursor: 0,
                },
            );
        }
    }
    Ok(())
}

fn handle_stat(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let dirfd = notif.data.args[0] as i32;
    let raw_path = match remote::read_path(notif.pid, notif.data.args[1]) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if raw_path.as_os_str().is_empty() && notif.data.args[3] as i32 & libc::AT_EMPTY_PATH != 0 {
        return respond_continue(listener, notif.id);
    }
    let virtual_path = match remote::resolve_path(state, notif.pid, dirfd, &raw_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.resolve_read(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = match CString::new(real_path.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(_) => return respond_error(listener, notif.id, libc::EINVAL),
    };
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::fstatat(
            libc::AT_FDCWD,
            path.as_ptr(),
            &mut stat,
            notif.data.args[3] as i32,
        )
    };
    if result < 0 {
        return respond_error(listener, notif.id, errno(&io::Error::last_os_error()));
    }
    match remote::write_memory(notif.pid, notif.data.args[2], seccomp::as_bytes(&stat)) {
        Ok(()) => respond_value(listener, notif.id, 0),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_statx(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let raw_path = match remote::read_path(notif.pid, notif.data.args[1]) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if raw_path.as_os_str().is_empty() && notif.data.args[2] as i32 & libc::AT_EMPTY_PATH != 0 {
        return respond_continue(listener, notif.id);
    }
    let virtual_path =
        match remote::resolve_path(state, notif.pid, notif.data.args[0] as i32, &raw_path) {
            Ok(path) => path,
            Err(error) => return respond_error(listener, notif.id, errno(&error)),
        };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.resolve_read(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = match CString::new(real_path.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(_) => return respond_error(listener, notif.id, libc::EINVAL),
    };
    let mut statx: libc::statx = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            notif.data.args[2] as i32,
            notif.data.args[3] as u32,
            &mut statx,
        )
    };
    if result < 0 {
        return respond_error(listener, notif.id, errno(&io::Error::last_os_error()));
    }
    match remote::write_memory(notif.pid, notif.data.args[4], seccomp::as_bytes(&statx)) {
        Ok(()) => respond_value(listener, notif.id, 0),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_access(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, mode, flags) = if syscall == sysno::ACCESS {
        (
            libc::AT_FDCWD,
            notif.data.args[0],
            notif.data.args[1] as i32,
            0,
        )
    } else {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2] as i32,
            notif.data.args[3] as i32,
        )
    };
    let virtual_path = match read_and_resolve(state, notif, dirfd, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.resolve_read(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = CString::new(real_path.as_os_str().as_bytes()).unwrap();
    let result = unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, flags) };
    if result < 0 {
        respond_error(listener, notif.id, errno(&io::Error::last_os_error()))
    } else {
        respond_value(listener, notif.id, 0)
    }
}

fn handle_mkdir(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, mode) = if syscall == sysno::MKDIR {
        (
            libc::AT_FDCWD,
            notif.data.args[0],
            notif.data.args[1] as u32,
        )
    } else {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2] as u32,
        )
    };
    let path = match read_and_resolve(state, notif, dirfd, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    match state.root.mkdir(&path, mode) {
        Ok(()) => respond_value(listener, notif.id, 0),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_unlink(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, flags) = if syscall == sysno::UNLINK {
        (libc::AT_FDCWD, notif.data.args[0], 0)
    } else if syscall == sysno::RMDIR {
        (libc::AT_FDCWD, notif.data.args[0], libc::AT_REMOVEDIR)
    } else {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2] as i32,
        )
    };
    let path = match read_and_resolve(state, notif, dirfd, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    match state.root.unlink(&path, flags & libc::AT_REMOVEDIR != 0) {
        Ok(()) => respond_value(listener, notif.id, 0),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_rename(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (old_dirfd, old_address, new_dirfd, new_address, flags) = if syscall == sysno::RENAME {
        (
            libc::AT_FDCWD,
            notif.data.args[0],
            libc::AT_FDCWD,
            notif.data.args[1],
            0,
        )
    } else {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2] as i32,
            notif.data.args[3],
            if syscall == libc::SYS_renameat2 {
                notif.data.args[4] as u32
            } else {
                0
            },
        )
    };
    if flags != 0 {
        return respond_error(listener, notif.id, libc::ENOTSUP);
    }
    let old = match read_and_resolve(state, notif, old_dirfd, old_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let new = match read_and_resolve(state, notif, new_dirfd, new_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    match state.root.rename(&old, &new) {
        Ok(()) => respond_value(listener, notif.id, 0),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_readlink(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, buffer, size) = if syscall == sysno::READLINK {
        (
            libc::AT_FDCWD,
            notif.data.args[0],
            notif.data.args[1],
            notif.data.args[2] as usize,
        )
    } else {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2],
            notif.data.args[3] as usize,
        )
    };
    let virtual_path = match read_and_resolve(state, notif, dirfd, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.resolve_read(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let target = match std::fs::read_link(real_path) {
        Ok(target) => target,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let bytes = target.as_os_str().as_bytes();
    let length = size.min(bytes.len());
    match remote::write_memory(notif.pid, buffer, &bytes[..length]) {
        Ok(()) => respond_value(listener, notif.id, length as i64),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_symlink(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let target = match remote::read_path(notif.pid, notif.data.args[0]) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let (dirfd, link_address) = if syscall == sysno::SYMLINK {
        (libc::AT_FDCWD, notif.data.args[1])
    } else {
        (notif.data.args[1] as i32, notif.data.args[2])
    };
    let link = match read_and_resolve(state, notif, dirfd, link_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    match state.root.symlink(&target, &link) {
        Ok(()) => respond_value(listener, notif.id, 0),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_getdents(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let fd = notif.data.args[0] as i32;
    let buffer = notif.data.args[1];
    let capacity = notif.data.args[2] as usize;
    let Some(directory) = state.directories.get_mut(&(notif.pid, fd)) else {
        return respond_continue(listener, notif.id);
    };
    let mut output = Vec::with_capacity(capacity.min(8192));
    while directory.cursor < directory.entries.len() {
        let entry = &directory.entries[directory.cursor];
        let record = dirent_record(entry, directory.cursor + 1);
        if output.len() + record.len() > capacity {
            break;
        }
        output.extend_from_slice(&record);
        directory.cursor += 1;
    }
    if output.is_empty() && directory.cursor < directory.entries.len() {
        return respond_error(listener, notif.id, libc::EINVAL);
    }
    match remote::write_memory(notif.pid, buffer, &output) {
        Ok(()) => respond_value(listener, notif.id, output.len() as i64),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_chdir(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let virtual_path = if syscall == libc::SYS_chdir {
        match read_and_resolve(state, notif, libc::AT_FDCWD, notif.data.args[0]) {
            Ok(path) => path,
            Err(error) => return respond_error(listener, notif.id, errno(&error)),
        }
    } else {
        let fd = notif.data.args[0] as i32;
        if let Some(directory) = state.directories.get(&(notif.pid, fd)) {
            directory.path.clone()
        } else {
            return respond_continue(listener, notif.id);
        }
    };

    if passthrough(&virtual_path) {
        state.virtual_cwds.insert(notif.pid, virtual_path);
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.resolve_read(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    match real_path.metadata() {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return respond_error(listener, notif.id, libc::ENOTDIR),
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    }
    state.virtual_cwds.insert(notif.pid, virtual_path.clone());

    // Let the kernel maintain cwd when the visible path is the host path. Upper-only
    // directories cannot be entered in the tracee's mount view, so emulate chdir and
    // use virtual_cwds for subsequent relative operations and getcwd.
    if real_path == virtual_path {
        respond_continue(listener, notif.id)
    } else {
        respond_value(listener, notif.id, 0)
    }
}

fn handle_getcwd(listener: RawFd, state: &State, notif: &SeccompNotif) -> io::Result<()> {
    let Some(path) = remote::virtual_cwd(state, notif.pid) else {
        return respond_continue(listener, notif.id);
    };
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    if bytes.len() > notif.data.args[1] as usize {
        return respond_error(listener, notif.id, libc::ERANGE);
    }
    match remote::write_memory(notif.pid, notif.data.args[0], &bytes) {
        Ok(()) => respond_value(listener, notif.id, bytes.len() as i64),
        Err(error) => respond_error(listener, notif.id, errno(&error)),
    }
}

fn handle_truncate(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let virtual_path = match read_and_resolve(state, notif, libc::AT_FDCWD, notif.data.args[0]) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.prepare_mutation(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = match c_path(&real_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, error),
    };
    let result = unsafe { libc::truncate(path.as_ptr(), notif.data.args[1] as libc::off_t) };
    respond_result(listener, notif.id, result)
}

fn handle_chmod(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, mode) = if syscall == sysno::CHMOD {
        (libc::AT_FDCWD, notif.data.args[0], notif.data.args[1])
    } else {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2],
        )
    };
    let virtual_path = match read_and_resolve(state, notif, dirfd, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.prepare_mutation(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = match c_path(&real_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, error),
    };
    let result = unsafe { libc::chmod(path.as_ptr(), mode as libc::mode_t) };
    respond_result(listener, notif.id, result)
}

fn handle_chown(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let syscall = notif.data.nr as i64;
    let (dirfd, path_address, owner, group, flags) = if syscall == libc::SYS_fchownat {
        (
            notif.data.args[0] as i32,
            notif.data.args[1],
            notif.data.args[2],
            notif.data.args[3],
            notif.data.args[4] as i32,
        )
    } else {
        (
            libc::AT_FDCWD,
            notif.data.args[0],
            notif.data.args[1],
            notif.data.args[2],
            if syscall == sysno::LCHOWN {
                libc::AT_SYMLINK_NOFOLLOW
            } else {
                0
            },
        )
    };
    let virtual_path = match read_and_resolve(state, notif, dirfd, path_address) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.prepare_mutation(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = match c_path(&real_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, error),
    };
    let result = unsafe {
        libc::fchownat(
            libc::AT_FDCWD,
            path.as_ptr(),
            owner as libc::uid_t,
            group as libc::gid_t,
            flags,
        )
    };
    respond_result(listener, notif.id, result)
}

fn handle_utimensat(listener: RawFd, state: &mut State, notif: &SeccompNotif) -> io::Result<()> {
    let virtual_path =
        match read_and_resolve(state, notif, notif.data.args[0] as i32, notif.data.args[1]) {
            Ok(path) => path,
            Err(error) => return respond_error(listener, notif.id, errno(&error)),
        };
    if passthrough(&virtual_path) {
        return respond_continue(listener, notif.id);
    }
    let real_path = match state.root.prepare_mutation(&virtual_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, errno(&error)),
    };
    let path = match c_path(&real_path) {
        Ok(path) => path,
        Err(error) => return respond_error(listener, notif.id, error),
    };
    let times = if notif.data.args[2] == 0 {
        None
    } else {
        match remote::read_value::<[libc::timespec; 2]>(notif.pid, notif.data.args[2]) {
            Ok(times) => Some(times),
            Err(error) => return respond_error(listener, notif.id, errno(&error)),
        }
    };
    let times_pointer = times
        .as_ref()
        .map_or(std::ptr::null(), |times| times.as_ptr());
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times_pointer,
            notif.data.args[3] as i32,
        )
    };
    respond_result(listener, notif.id, result)
}

fn dirent_record(entry: &crate::root::DirEntry, next_offset: usize) -> Vec<u8> {
    let name = entry.name.as_bytes();
    // linux_dirent64 keeps d_type before the flexible d_name field.
    let raw_length = 8 + 8 + 2 + 1 + name.len() + 1;
    let record_length = (raw_length + 7) & !7;
    let mut record = vec![0u8; record_length];
    record[0..8].copy_from_slice(&entry.inode.to_ne_bytes());
    record[8..16].copy_from_slice(&(next_offset as i64).to_ne_bytes());
    record[16..18].copy_from_slice(&(record_length as u16).to_ne_bytes());
    record[18] = entry.file_type;
    record[19..19 + name.len()].copy_from_slice(name);
    record[19 + name.len()] = 0;
    record
}

fn read_and_resolve(
    state: &State,
    notif: &SeccompNotif,
    dirfd: i32,
    address: u64,
) -> io::Result<PathBuf> {
    let path = remote::read_path(notif.pid, address)?;
    remote::resolve_path(state, notif.pid, dirfd, &path)
}

fn c_path(path: &Path) -> Result<CString, i32> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)
}

fn passthrough(path: &Path) -> bool {
    ["/dev", "/proc", "/sys"]
        .iter()
        .any(|prefix| path == Path::new(prefix) || path.starts_with(prefix))
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

fn respond_value(listener: RawFd, id: u64, value: i64) -> io::Result<()> {
    seccomp::respond(
        listener,
        &SeccompNotifResp {
            id,
            val: value,
            error: 0,
            flags: 0,
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

fn respond_result(listener: RawFd, id: u64, result: i32) -> io::Result<()> {
    if result < 0 {
        respond_error(listener, id, errno(&io::Error::last_os_error()))
    } else {
        respond_value(listener, id, result as i64)
    }
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}
