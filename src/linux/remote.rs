use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};

use super::State;

const MAX_PATH: usize = 4096;

pub(crate) fn read_path(pid: u32, address: u64) -> io::Result<PathBuf> {
    if address == 0 {
        return Err(io::Error::from_raw_os_error(libc::EFAULT));
    }
    let mut bytes = Vec::with_capacity(256);
    while bytes.len() < MAX_PATH {
        let chunk_len = (MAX_PATH - bytes.len()).min(256);
        let chunk = read_memory(pid, address + bytes.len() as u64, chunk_len)?;
        if let Some(end) = chunk.iter().position(|byte| *byte == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return Ok(PathBuf::from(OsString::from_vec(bytes)));
        }
        bytes.extend_from_slice(&chunk);
    }
    Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG))
}

pub(crate) fn read_value<T: Copy>(pid: u32, address: u64) -> io::Result<T> {
    let bytes = read_memory(pid, address, std::mem::size_of::<T>())?;
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) })
}

pub(crate) fn write_memory(pid: u32, address: u64, bytes: &[u8]) -> io::Result<()> {
    let local = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let remote = libc::iovec {
        iov_base: address as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let written = unsafe { libc::process_vm_writev(pid as i32, &local, 1, &remote, 1, 0) };
    if written < 0 {
        Err(io::Error::last_os_error())
    } else if written as usize != bytes.len() {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short process_vm_writev",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn rewrite_path(
    pid: u32,
    address: u64,
    original: &Path,
    replacement: &Path,
) -> io::Result<()> {
    let mut bytes = replacement.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    if bytes.len() > original.as_os_str().as_bytes().len() + 1 {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    match write_memory(pid, address, &bytes) {
        Ok(()) => Ok(()),
        Err(_) => write_process_memory(pid, address, &bytes),
    }
}

fn write_process_memory(pid: u32, address: u64, bytes: &[u8]) -> io::Result<()> {
    // process_vm_writev honors the tracee page's write bit, while an exec
    // pathname may live in read-only program data. /proc/pid/mem provides the
    // same parent-child access boundary but can update that private mapping.
    let memory = OpenOptions::new()
        .write(true)
        .open(format!("/proc/{pid}/mem"))?;
    let written = memory.write_at(bytes, address)?;
    if written == bytes.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short /proc/pid/mem write",
        ))
    }
}

pub(crate) fn resolve_path(
    state: &State,
    pid: u32,
    dirfd: i32,
    path: &Path,
) -> io::Result<PathBuf> {
    // An empty pathname is not a relative spelling of the working directory.
    // Syscalls that support AT_EMPTY_PATH handle it before reaching this helper;
    // ordinary pathname operations must preserve the kernel's ENOENT semantics.
    if path.as_os_str().is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    if path.is_absolute() {
        return Ok(normalize(path));
    }
    let base = if dirfd == libc::AT_FDCWD {
        virtual_cwd(state, pid).unwrap_or_else(|| process_cwd(pid, state))
    } else if let Some(directory) = state
        .directories
        .get(&(process_key(pid), dirfd))
        .filter(|directory| directory_fd_matches(state, pid, dirfd, directory))
    {
        directory.path.clone()
    } else {
        let host = std::fs::read_link(format!("/proc/{pid}/fd/{dirfd}"))?;
        state.view.virtual_for_host(&host)
    };
    Ok(normalize(&base.join(path)))
}

pub(crate) fn directory_fd_matches(
    state: &State,
    pid: u32,
    fd: i32,
    directory: &super::OpenDirectory,
) -> bool {
    let Ok(actual) = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")) else {
        return false;
    };
    let Ok(expected) = state.view.resolve_read(&directory.path) else {
        return false;
    };
    // The tracee can close a projected directory and later reuse the same fd for
    // an unrelated path. Seccomp notifications do not provide an after-close
    // hook, so validate the live fd before trusting our fd-indexed virtual state.
    std::fs::canonicalize(&expected).unwrap_or(expected) == actual
}

pub(crate) fn virtual_cwd(state: &State, pid: u32) -> Option<PathBuf> {
    let mut current = process_key(pid);
    for _ in 0..16 {
        if let Some(path) = state.virtual_cwds.get(&current) {
            return Some(path.clone());
        }
        let parent = process_parent(current)?;
        if parent == 0 || parent == current {
            break;
        }
        current = process_key(parent);
    }
    None
}

pub(crate) fn process_key(pid: u32) -> u32 {
    process_status_field(pid, "Tgid:").unwrap_or(pid)
}

pub(crate) fn is_own_proc_cwd(path: &Path, pid: u32) -> bool {
    if path == Path::new("/proc/self/cwd") || path == Path::new("/proc/thread-self/cwd") {
        return true;
    }
    let Some(target) = path
        .strip_prefix("/proc")
        .ok()
        .and_then(|path| path.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return false;
    };
    path == Path::new(&format!("/proc/{target}/cwd"))
        && (target == pid || target == process_key(pid))
}

fn read_memory(pid: u32, address: u64, length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0u8; length];
    let local = libc::iovec {
        iov_base: bytes.as_mut_ptr() as *mut libc::c_void,
        iov_len: length,
    };
    let remote = libc::iovec {
        iov_base: address as *mut libc::c_void,
        iov_len: length,
    };
    let read = unsafe { libc::process_vm_readv(pid as i32, &local, 1, &remote, 1, 0) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(read as usize);
    Ok(bytes)
}

fn process_cwd(pid: u32, state: &State) -> PathBuf {
    let host =
        std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap_or_else(|_| PathBuf::from("/"));
    state.view.virtual_for_host(&host)
}

fn process_parent(pid: u32) -> Option<u32> {
    process_status_field(pid, "PPid:")
}

fn process_status_field(pid: u32, name: &str) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))?
        .trim()
        .parse()
        .ok()
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) => {}
        }
    }
    normalized
}
