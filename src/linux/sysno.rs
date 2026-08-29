// Modern Linux architectures expose only the *at filesystem syscalls. x86_64
// keeps older path-only entrypoints for compatibility, so intercept those there
// without making the portable dispatch code reference absent libc constants.
#[cfg(target_arch = "x86_64")]
mod legacy {
    pub(crate) const OPEN: i64 = libc::SYS_open;
    pub(crate) const ACCESS: i64 = libc::SYS_access;
    pub(crate) const MKDIR: i64 = libc::SYS_mkdir;
    pub(crate) const UNLINK: i64 = libc::SYS_unlink;
    pub(crate) const RMDIR: i64 = libc::SYS_rmdir;
    pub(crate) const RENAME: i64 = libc::SYS_rename;
    pub(crate) const READLINK: i64 = libc::SYS_readlink;
    pub(crate) const SYMLINK: i64 = libc::SYS_symlink;
    pub(crate) const CHMOD: i64 = libc::SYS_chmod;
    pub(crate) const CHOWN: i64 = libc::SYS_chown;
    pub(crate) const LCHOWN: i64 = libc::SYS_lchown;
}

#[cfg(not(target_arch = "x86_64"))]
mod legacy {
    pub(crate) const OPEN: i64 = -1;
    pub(crate) const ACCESS: i64 = -1;
    pub(crate) const MKDIR: i64 = -1;
    pub(crate) const UNLINK: i64 = -1;
    pub(crate) const RMDIR: i64 = -1;
    pub(crate) const RENAME: i64 = -1;
    pub(crate) const READLINK: i64 = -1;
    pub(crate) const SYMLINK: i64 = -1;
    pub(crate) const CHMOD: i64 = -1;
    pub(crate) const CHOWN: i64 = -1;
    pub(crate) const LCHOWN: i64 = -1;
}

pub(crate) use legacy::*;
