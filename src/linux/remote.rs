use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
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

pub(crate) fn resolve_path(
    state: &State,
    pid: u32,
    dirfd: i32,
    path: &Path,
) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(normalize(path));
    }
    let base = if dirfd == libc::AT_FDCWD {
        virtual_cwd(state, pid).unwrap_or_else(|| process_cwd(pid, state))
    } else if let Some(directory) = state.directories.get(&(pid, dirfd)) {
        directory.path.clone()
    } else {
        let host = std::fs::read_link(format!("/proc/{pid}/fd/{dirfd}"))?;
        host_to_virtual(&host, state.root.upper())
    };
    Ok(normalize(&base.join(path)))
}

pub(crate) fn virtual_cwd(state: &State, pid: u32) -> Option<PathBuf> {
    let mut current = pid;
    for _ in 0..16 {
        if let Some(path) = state.virtual_cwds.get(&current) {
            return Some(path.clone());
        }
        let parent = process_parent(current)?;
        if parent == 0 || parent == current {
            break;
        }
        current = parent;
    }
    None
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
    host_to_virtual(&host, state.root.upper())
}

fn process_parent(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()
}

fn host_to_virtual(host: &Path, upper: &Path) -> PathBuf {
    host.strip_prefix(upper)
        .map(|relative| Path::new("/").join(relative))
        .unwrap_or_else(|_| host.to_path_buf())
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
