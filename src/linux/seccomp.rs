use std::io;
use std::mem::{self, size_of};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::sysno;

pub(crate) const USER_NOTIF: u32 = 0x7fc0_0000;
pub(crate) const USER_NOTIF_FLAG_CONTINUE: u32 = 1;
pub(crate) const ADDFD_FLAG_SEND: u32 = 2;

const ALLOW: u32 = 0x7fff_0000;
const SET_MODE_FILTER: libc::c_ulong = 1;
const FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SeccompData {
    pub(crate) nr: i32,
    pub(crate) arch: u32,
    pub(crate) instruction_pointer: u64,
    pub(crate) args: [u64; 6],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SeccompNotif {
    pub(crate) id: u64,
    pub(crate) pid: u32,
    pub(crate) flags: u32,
    pub(crate) data: SeccompData,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SeccompNotifResp {
    pub(crate) id: u64,
    pub(crate) val: i64,
    pub(crate) error: i32,
    pub(crate) flags: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SeccompNotifAddfd {
    pub(crate) id: u64,
    pub(crate) flags: u32,
    pub(crate) srcfd: u32,
    pub(crate) newfd: u32,
    pub(crate) newfd_flags: u32,
}

pub(crate) fn install_listener() -> io::Result<OwnedFd> {
    let syscalls = intercepted_syscalls();
    let mut filter = Vec::with_capacity(2 + syscalls.len() * 2);
    filter.push(stmt(BPF_LD | BPF_W | BPF_ABS, 0));
    for syscall in syscalls {
        filter.push(jump(BPF_JMP | BPF_JEQ | BPF_K, syscall as u32, 0, 1));
        filter.push(stmt(BPF_RET | BPF_K, USER_NOTIF));
    }
    filter.push(stmt(BPF_RET | BPF_K, ALLOW));

    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    let no_new_privs = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if no_new_privs != 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SET_MODE_FILTER,
            FILTER_FLAG_NEW_LISTENER,
            &program as *const libc::sock_fprog,
        ) as i32
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) fn receive(listener: RawFd) -> io::Result<SeccompNotif> {
    let mut notification = SeccompNotif::default();
    let result = unsafe { libc::ioctl(listener, notif_recv_ioctl(), &mut notification) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(notification)
    }
}

pub(crate) fn respond(listener: RawFd, response: &SeccompNotifResp) -> io::Result<()> {
    let result = unsafe { libc::ioctl(listener, notif_send_ioctl(), response) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn add_fd(listener: RawFd, request: &SeccompNotifAddfd) -> io::Result<i32> {
    let result = unsafe { libc::ioctl(listener, notif_addfd_ioctl(), request) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn intercepted_syscalls() -> Vec<i64> {
    vec![
        sysno::OPEN,
        libc::SYS_openat,
        libc::SYS_openat2,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        sysno::ACCESS,
        libc::SYS_faccessat,
        libc::SYS_faccessat2,
        sysno::MKDIR,
        libc::SYS_mkdirat,
        sysno::UNLINK,
        libc::SYS_unlinkat,
        sysno::RMDIR,
        sysno::RENAME,
        libc::SYS_renameat,
        libc::SYS_renameat2,
        sysno::READLINK,
        libc::SYS_readlinkat,
        sysno::SYMLINK,
        libc::SYS_symlinkat,
        libc::SYS_getdents64,
        libc::SYS_chdir,
        libc::SYS_fchdir,
        libc::SYS_getcwd,
        libc::SYS_truncate,
        sysno::CHMOD,
        libc::SYS_fchmodat,
        sysno::CHOWN,
        sysno::LCHOWN,
        libc::SYS_fchownat,
        libc::SYS_utimensat,
    ]
    .into_iter()
    .filter(|syscall| *syscall >= 0)
    .collect()
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const fn ioc(direction: u64, ty: u8, number: u8, size: usize) -> libc::c_ulong {
    ((direction << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((number as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)) as libc::c_ulong
}

const fn notif_recv_ioctl() -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, b'!', 0, size_of::<SeccompNotif>())
}

const fn notif_send_ioctl() -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, b'!', 1, size_of::<SeccompNotifResp>())
}

const fn notif_addfd_ioctl() -> libc::c_ulong {
    ioc(IOC_WRITE, b'!', 3, size_of::<SeccompNotifAddfd>())
}

pub(crate) fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, mem::size_of::<T>()) }
}
