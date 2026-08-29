#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

const META_DIR: &str = ".pathshim";
const WHITEOUT_LOG: &str = "whiteouts";

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct BindSpec {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
}

#[derive(Debug)]
// Filesystem-view contract: one invocation resolves every guest path against one
// deterministic projection set. The supervisor shares this RootView across the
// command tree so writes and whiteouts are immediately visible to later syscalls.
// A projection source has one active RootView owner; independent pathshim
// invocations must not concurrently share it.
pub(crate) struct RootView {
    projections: Vec<Projection>,
}

#[derive(Debug)]
struct Projection {
    source: PathBuf,
    destination: PathBuf,
    whiteouts: Whiteouts,
}

#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct DirEntry {
    pub(crate) name: OsString,
    pub(crate) inode: u64,
    pub(crate) file_type: u8,
}

impl RootView {
    #[cfg(test)]
    pub(crate) fn open(upper: &Path) -> io::Result<Self> {
        Self::open_with_binds(Some(upper), &[])
    }

    pub(crate) fn open_with_binds(rootfs: Option<&Path>, binds: &[BindSpec]) -> io::Result<Self> {
        let mut specs = Vec::with_capacity(binds.len() + usize::from(rootfs.is_some()));
        if let Some(source) = rootfs {
            specs.push(BindSpec {
                source: source.to_path_buf(),
                destination: PathBuf::from("/"),
            });
        }
        specs.extend_from_slice(binds);

        let mut destinations = HashSet::new();
        let mut projections = Vec::with_capacity(specs.len());
        for spec in specs {
            if !spec.destination.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "bind destination must be absolute: {}",
                        spec.destination.display()
                    ),
                ));
            }
            let destination = normalize_virtual(&spec.destination)?;
            if is_reserved(&destination) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "bind destination is reserved for host passthrough: {}",
                        destination.display()
                    ),
                ));
            }
            if !destinations.insert(destination.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("duplicate bind destination: {}", destination.display()),
                ));
            }
            projections.push(Projection::open(&spec.source, destination)?);
        }
        projections.sort_by_key(|projection| {
            std::cmp::Reverse(projection.destination.components().count())
        });
        Ok(Self { projections })
    }

    pub(crate) fn reopen(&self) -> io::Result<Self> {
        let binds: Vec<_> = self
            .projections
            .iter()
            .map(|projection| BindSpec {
                source: projection.source.clone(),
                destination: projection.destination.clone(),
            })
            .collect();
        Self::open_with_binds(None, &binds)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn upper(&self) -> &Path {
        self.root_upper()
            .expect("upper is only available for a root projection")
    }

    pub(crate) fn root_upper(&self) -> Option<&Path> {
        self.projections
            .iter()
            .find(|projection| projection.destination == Path::new("/"))
            .map(|projection| projection.source.as_path())
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn projection_count(&self) -> usize {
        self.projections.len()
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn probe_path(&self) -> &Path {
        &self
            .projections
            .first()
            .expect("a filesystem view has at least one projection")
            .destination
    }

    pub(crate) fn projects(&self, virtual_path: &Path) -> bool {
        normalize_virtual(virtual_path)
            .ok()
            .is_some_and(|path| self.projection_index(&path).is_some())
    }

    pub(crate) fn same_projection(&self, first: &Path, second: &Path) -> bool {
        let Ok(first) = normalize_virtual(first) else {
            return false;
        };
        let Ok(second) = normalize_virtual(second) else {
            return false;
        };
        matches!(
            (self.projection_index(&first), self.projection_index(&second)),
            (Some(first), Some(second)) if first == second
        )
    }

    pub(crate) fn resolve_read(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        match self.projection_index(&virtual_path) {
            Some(index) => self.projections[index].resolve_read(&virtual_path),
            None => Ok(virtual_path),
        }
    }

    pub(crate) fn resolve_directory(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let real_path = self.resolve_read(&virtual_path)?;
        match real_path.metadata() {
            Ok(metadata) if metadata.is_dir() => Ok(virtual_path),
            Ok(_) => Err(io::Error::from_raw_os_error(libc_errno::ENOTDIR)),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn create_directory_all(&mut self, virtual_path: &Path) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let mut current = PathBuf::from("/");
        for component in relative(&virtual_path).components() {
            current.push(component);
            if self.entry_exists(&current) {
                let real_path = self.resolve_read(&current)?;
                if !real_path.metadata()?.is_dir() {
                    return Err(io::Error::from_raw_os_error(libc_errno::ENOTDIR));
                }
                continue;
            }
            self.mkdir(&current, 0o755)?;
        }
        Ok(virtual_path)
    }

    pub(crate) fn materialize_directory(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let Some(root) = self
            .projections
            .iter()
            .find(|projection| projection.destination == Path::new("/"))
        else {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOTSUP));
        };
        let real_path = root.source.join(relative(&virtual_path));
        fs::create_dir_all(&real_path)?;
        Ok(real_path)
    }

    pub(crate) fn prepare_open(&mut self, virtual_path: &Path, flags: i32) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        match self.projection_index(&virtual_path) {
            Some(index) => self.projections[index].prepare_open(&virtual_path, flags),
            None => Ok(virtual_path),
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn prepare_mutation(&mut self, virtual_path: &Path) -> io::Result<PathBuf> {
        self.prepare_open(virtual_path, libc_flags::O_WRONLY)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn mkdir(&mut self, virtual_path: &Path, mode: u32) -> io::Result<()> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let Some(index) = self.projection_index(&virtual_path) else {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOTSUP));
        };
        self.projections[index].mkdir(&virtual_path, mode)
    }

    pub(crate) fn unlink(&mut self, virtual_path: &Path, remove_dir: bool) -> io::Result<()> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let Some(index) = self.projection_index(&virtual_path) else {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOTSUP));
        };
        self.projections[index].unlink(&virtual_path, remove_dir)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn rename(&mut self, old: &Path, new: &Path) -> io::Result<()> {
        let old = normalize_virtual(old)?;
        let new = normalize_virtual(new)?;
        let (Some(old_index), Some(new_index)) =
            (self.projection_index(&old), self.projection_index(&new))
        else {
            return Err(io::Error::from_raw_os_error(libc_errno::EXDEV));
        };
        if old_index != new_index {
            return Err(io::Error::from_raw_os_error(libc_errno::EXDEV));
        }
        self.projections[old_index].rename(&old, &new)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn symlink(&mut self, target: &Path, link: &Path) -> io::Result<()> {
        let link = normalize_virtual(link)?;
        let Some(index) = self.projection_index(&link) else {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOTSUP));
        };
        self.projections[index].symlink(target, &link)
    }

    pub(crate) fn list_dir(&self, virtual_path: &Path) -> io::Result<Vec<DirEntry>> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let Some(index) = self.projection_index(&virtual_path) else {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOTSUP));
        };
        self.projections[index].list_dir(&virtual_path)
    }

    fn projection_index(&self, virtual_path: &Path) -> Option<usize> {
        if is_reserved(virtual_path) {
            return None;
        }
        self.projections.iter().position(|projection| {
            virtual_path == projection.destination
                || virtual_path.starts_with(&projection.destination)
        })
    }

    fn entry_exists(&self, virtual_path: &Path) -> bool {
        match self.projection_index(virtual_path) {
            Some(index) => self.projections[index].entry_exists(virtual_path),
            None => virtual_path.symlink_metadata().is_ok(),
        }
    }
}

impl Projection {
    fn open(source: &Path, destination: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(source)?;
        let source = fs::canonicalize(source)?;
        let meta = source.join(META_DIR);
        fs::create_dir_all(&meta)?;
        let whiteouts = Whiteouts::load(meta.join(WHITEOUT_LOG))?;
        Ok(Self {
            source,
            destination,
            whiteouts,
        })
    }

    fn relative<'a>(&self, virtual_path: &'a Path) -> &'a Path {
        virtual_path
            .strip_prefix(&self.destination)
            .expect("projection selected by destination prefix")
    }

    fn resolve_read(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        let rel = self.relative(virtual_path);
        if self.whiteouts.covers(rel) {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOENT));
        }
        let upper = self.source.join(rel);
        if upper.symlink_metadata().is_ok() {
            Ok(upper)
        } else {
            Ok(virtual_path.to_path_buf())
        }
    }

    fn prepare_open(&mut self, virtual_path: &Path, flags: i32) -> io::Result<PathBuf> {
        let write = flags & write_flags() != 0;
        if !write {
            return self.resolve_read(virtual_path);
        }

        let rel = self.relative(virtual_path);
        let upper = self.source.join(rel);
        let merged_exists = !self.whiteouts.covers(rel)
            && (upper.symlink_metadata().is_ok() || virtual_path.symlink_metadata().is_ok());
        if flags & libc_flags::O_CREAT != 0 && flags & libc_flags::O_EXCL != 0 && merged_exists {
            return Err(io::Error::from_raw_os_error(libc_errno::EEXIST));
        }

        self.ensure_parent(virtual_path)?;
        if upper.symlink_metadata().is_err() && !self.whiteouts.covers(rel) {
            self.copy_lower_entry(virtual_path, &upper)?;
        }
        self.whiteouts.reveal(rel)?;
        Ok(upper)
    }

    fn mkdir(&mut self, virtual_path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;

        let rel = self.relative(virtual_path);
        if self.entry_exists(virtual_path) {
            return Err(io::Error::from_raw_os_error(libc_errno::EEXIST));
        }
        self.ensure_parent(virtual_path)?;
        fs::DirBuilder::new()
            .mode(mode)
            .create(self.source.join(rel))?;
        self.whiteouts.reveal(rel)
    }

    fn unlink(&mut self, virtual_path: &Path, remove_dir: bool) -> io::Result<()> {
        let rel = self.relative(virtual_path);
        let upper = self.source.join(rel);
        let lower_exists = virtual_path.symlink_metadata().is_ok();
        let upper_metadata = upper.symlink_metadata().ok();
        if upper_metadata.is_none() && (!lower_exists || self.whiteouts.covers(rel)) {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOENT));
        }

        if let Some(metadata) = upper_metadata {
            if remove_dir {
                if !metadata.is_dir() {
                    return Err(io::Error::from_raw_os_error(libc_errno::ENOTDIR));
                }
                fs::remove_dir(&upper)?;
            } else {
                if metadata.is_dir() {
                    return Err(io::Error::from_raw_os_error(libc_errno::EISDIR));
                }
                fs::remove_file(&upper)?;
            }
        }
        if lower_exists {
            self.whiteouts.hide(rel)?;
        } else {
            self.whiteouts.reveal(rel)?;
        }
        Ok(())
    }

    fn rename(&mut self, old: &Path, new: &Path) -> io::Result<()> {
        if !self.entry_exists(old) {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOENT));
        }
        let old_rel = self.relative(old);
        let new_rel = self.relative(new);
        self.ensure_parent(new)?;
        let old_upper = self.source.join(old_rel);
        if old_upper.symlink_metadata().is_err() {
            self.copy_lower_entry_recursive(old, &old_upper)?;
        }
        let new_upper = self.source.join(new_rel);
        if let Ok(metadata) = new_upper.symlink_metadata() {
            if metadata.is_dir() {
                fs::remove_dir_all(&new_upper)?;
            } else {
                fs::remove_file(&new_upper)?;
            }
        }
        fs::rename(&old_upper, &new_upper)?;
        if old.symlink_metadata().is_ok() {
            self.whiteouts.hide(old_rel)?;
        } else {
            self.whiteouts.reveal(old_rel)?;
        }
        self.whiteouts.reveal(new_rel)
    }

    fn symlink(&mut self, target: &Path, link: &Path) -> io::Result<()> {
        use std::os::unix::fs::symlink;

        if self.entry_exists(link) {
            return Err(io::Error::from_raw_os_error(libc_errno::EEXIST));
        }
        self.ensure_parent(link)?;
        let rel = self.relative(link);
        symlink(target, self.source.join(rel))?;
        self.whiteouts.reveal(rel)
    }

    fn list_dir(&self, virtual_path: &Path) -> io::Result<Vec<DirEntry>> {
        use std::os::unix::fs::MetadataExt;

        let rel = self.relative(virtual_path);
        if self.whiteouts.covers(rel) {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOENT));
        }

        let mut entries = BTreeMap::<OsString, DirEntry>::new();
        let upper = self.source.join(rel);
        for base in [virtual_path, upper.as_path()] {
            let Ok(read_dir) = fs::read_dir(base) else {
                continue;
            };
            for entry in read_dir.flatten() {
                let name = entry.file_name();
                if virtual_path == self.destination && name == META_DIR {
                    continue;
                }
                let child_rel = rel.join(&name);
                if self.whiteouts.covers(&child_rel) {
                    entries.remove(&name);
                    continue;
                }
                let Ok(metadata) = entry.path().symlink_metadata() else {
                    continue;
                };
                entries.insert(
                    name.clone(),
                    DirEntry {
                        name,
                        inode: metadata.ino(),
                        file_type: metadata_to_dir_type(&metadata),
                    },
                );
            }
        }
        Ok(entries.into_values().collect())
    }

    fn entry_exists(&self, virtual_path: &Path) -> bool {
        let rel = self.relative(virtual_path);
        !self.whiteouts.covers(rel)
            && (self.source.join(rel).symlink_metadata().is_ok()
                || virtual_path.symlink_metadata().is_ok())
    }

    fn ensure_parent(&self, virtual_path: &Path) -> io::Result<()> {
        let Some(parent) = virtual_path.parent() else {
            return Ok(());
        };
        if !self.entry_exists(parent) {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOENT));
        }
        let upper_parent = self.source.join(self.relative(parent));
        fs::create_dir_all(upper_parent)
    }

    fn copy_lower_entry(&self, lower: &Path, upper: &Path) -> io::Result<()> {
        let Ok(metadata) = lower.symlink_metadata() else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() {
            match lower.metadata() {
                Ok(target) if target.is_file() => fs::copy(lower, upper).map(|_| ()),
                Ok(target) if target.is_dir() => fs::create_dir(upper),
                _ => std::os::unix::fs::symlink(fs::read_link(lower)?, upper),
            }
        } else if metadata.is_file() {
            fs::copy(lower, upper).map(|_| ())
        } else if metadata.is_dir() {
            fs::create_dir(upper)
        } else {
            // Devices, sockets, and FIFOs keep their host semantics.
            Ok(())
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn copy_lower_entry_recursive(&self, lower: &Path, upper: &Path) -> io::Result<()> {
        let metadata = lower.symlink_metadata()?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return self.copy_lower_entry(lower, upper);
        }
        fs::create_dir_all(upper)?;
        for entry in fs::read_dir(lower)? {
            let entry = entry?;
            self.copy_lower_entry_recursive(&entry.path(), &upper.join(entry.file_name()))?;
        }
        Ok(())
    }
}

fn normalize_virtual(path: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported path prefix",
                ));
            }
        }
    }
    Ok(out)
}

fn relative(path: &Path) -> &Path {
    path.strip_prefix("/").unwrap_or(path)
}

fn is_reserved(path: &Path) -> bool {
    ["/dev", "/proc", "/sys"]
        .iter()
        .any(|prefix| path == Path::new(prefix) || path.starts_with(prefix))
}

fn write_flags() -> i32 {
    libc_flags::O_WRONLY
        | libc_flags::O_RDWR
        | libc_flags::O_CREAT
        | libc_flags::O_TRUNC
        | libc_flags::O_APPEND
}

fn metadata_to_dir_type(metadata: &fs::Metadata) -> u8 {
    let kind = metadata.file_type();
    if kind.is_dir() {
        4
    } else if kind.is_file() {
        8
    } else if kind.is_symlink() {
        10
    } else {
        0
    }
}

#[derive(Debug)]
struct Whiteouts {
    hidden: HashSet<Vec<u8>>,
    log: File,
}

impl Whiteouts {
    fn load(path: PathBuf) -> io::Result<Self> {
        let mut hidden = HashSet::new();
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if line.is_empty() {
                    continue;
                }
                let (operation, encoded) = line.split_at(1);
                let Some(path) = hex_decode(encoded.as_bytes()) else {
                    continue;
                };
                if operation == "+" {
                    hidden.insert(path);
                } else if operation == "-" {
                    hidden.remove(&path);
                }
            }
        }
        let log = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { hidden, log })
    }

    fn covers(&self, rel: &Path) -> bool {
        let bytes = rel.as_os_str().as_bytes();
        self.hidden.iter().any(|hidden| {
            bytes == hidden.as_slice()
                || (bytes.starts_with(hidden) && bytes.get(hidden.len()).copied() == Some(b'/'))
        })
    }

    fn hide(&mut self, rel: &Path) -> io::Result<()> {
        let bytes = rel.as_os_str().as_bytes().to_vec();
        if self.hidden.insert(bytes.clone()) {
            self.append(b'+', &bytes)?;
        }
        Ok(())
    }

    fn reveal(&mut self, rel: &Path) -> io::Result<()> {
        let bytes = rel.as_os_str().as_bytes().to_vec();
        let removed: Vec<Vec<u8>> = self
            .hidden
            .iter()
            .filter(|hidden| {
                bytes == **hidden
                    || (bytes.starts_with(hidden.as_slice())
                        && bytes.get(hidden.len()).copied() == Some(b'/'))
            })
            .cloned()
            .collect();
        for hidden in removed {
            self.hidden.remove(&hidden);
            self.append(b'-', &hidden)?;
        }
        Ok(())
    }

    fn append(&mut self, operation: u8, path: &[u8]) -> io::Result<()> {
        self.log.write_all(&[operation])?;
        self.log.write_all(hex_encode(path).as_bytes())?;
        self.log.write_all(b"\n")?;
        self.log.flush()
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn hex_decode(encoded: &[u8]) -> Option<Vec<u8>> {
    if encoded.len() % 2 != 0 {
        return None;
    }
    encoded
        .chunks_exact(2)
        .map(|pair| Some((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

// Keep the pure path/COW layer buildable on macOS while the syscall backend is Linux-only.
#[cfg(target_os = "linux")]
mod libc_flags {
    pub(super) use libc::{O_APPEND, O_CREAT, O_EXCL, O_RDWR, O_TRUNC, O_WRONLY};
}

#[cfg(not(target_os = "linux"))]
mod libc_flags {
    pub(super) const O_WRONLY: i32 = 1;
    pub(super) const O_RDWR: i32 = 2;
    pub(super) const O_CREAT: i32 = 0x200;
    pub(super) const O_TRUNC: i32 = 0x400;
    pub(super) const O_APPEND: i32 = 0x8;
    pub(super) const O_EXCL: i32 = 0x800;
}

mod libc_errno {
    pub(super) const ENOENT: i32 = 2;
    pub(super) const EXDEV: i32 = 18;
    pub(super) const EEXIST: i32 = 17;
    pub(super) const ENOTDIR: i32 = 20;
    pub(super) const EISDIR: i32 = 21;
    pub(super) const ENOTSUP: i32 = 95;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("pathshim-root-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test directory");
        }
    }

    #[test]
    fn reads_upper_first_and_lower_as_fallback() {
        let temp = TempDir::new();
        let mut root = RootView::open(&temp.0).unwrap();
        let upper_root = fs::canonicalize(&temp.0).unwrap();
        fs::write(temp.0.join("upper.txt"), "upper").unwrap();

        assert_eq!(
            root.resolve_read(Path::new("/upper.txt")).unwrap(),
            upper_root.join("upper.txt")
        );
        assert_eq!(
            root.resolve_read(Path::new("/bin/sh")).unwrap(),
            PathBuf::from("/bin/sh")
        );

        let copied = root
            .prepare_open(Path::new("/bin/sh"), libc_flags::O_RDWR)
            .unwrap();
        assert_eq!(copied, upper_root.join("bin/sh"));
        assert!(copied.is_file());
    }

    #[test]
    fn creates_missing_rootfs_without_assuming_a_layout() {
        let temp = TempDir::new();
        let rootfs = temp.0.join("missing/rootfs");

        let root = RootView::open(&rootfs).unwrap();

        assert_eq!(root.upper(), fs::canonicalize(&rootfs).unwrap());
        assert!(root.upper().join(META_DIR).is_dir());
        assert!(!root.upper().join("project").exists());
    }

    #[test]
    fn resolves_and_materializes_guest_directories() {
        let temp = TempDir::new();
        let root = RootView::open(&temp.0).unwrap();
        fs::create_dir_all(temp.0.join("workspace")).unwrap();

        assert_eq!(
            root.resolve_directory(Path::new("workspace/../workspace"))
                .unwrap(),
            PathBuf::from("/workspace")
        );
        assert_eq!(
            root.materialize_directory(Path::new("/lower-only/cwd"))
                .unwrap(),
            root.upper().join("lower-only/cwd")
        );
    }

    #[test]
    fn creates_missing_guest_directory_without_shadowing_files() {
        let temp = TempDir::new();
        let mut root = RootView::open(&temp.0).unwrap();
        fs::write(temp.0.join("file"), "not a directory").unwrap();

        assert_eq!(
            root.create_directory_all(Path::new("/workspace/nested"))
                .unwrap(),
            PathBuf::from("/workspace/nested")
        );
        assert!(temp.0.join("workspace/nested").is_dir());
        assert_eq!(
            root.create_directory_all(Path::new("/file/nested"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc_errno::ENOTDIR)
        );
    }

    #[test]
    fn rejects_missing_guest_directory() {
        let temp = TempDir::new();
        let root = RootView::open(&temp.0).unwrap();

        assert_eq!(
            root.resolve_directory(Path::new("/pathshim-missing-cwd"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc_errno::ENOENT)
        );
    }

    #[test]
    fn whiteout_persists_across_root_view_instances() {
        let temp = TempDir::new();
        {
            let mut root = RootView::open(&temp.0).unwrap();
            root.unlink(Path::new("/bin/sh"), false).unwrap();
            assert!(root.resolve_read(Path::new("/bin/sh")).is_err());
        }
        let root = RootView::open(&temp.0).unwrap();
        assert!(root.resolve_read(Path::new("/bin/sh")).is_err());
    }

    #[test]
    fn directory_listing_merges_both_layers() {
        let temp = TempDir::new();
        let root = RootView::open(&temp.0).unwrap();
        fs::create_dir_all(temp.0.join("tmp")).unwrap();
        fs::write(temp.0.join("tmp/pathshim-only"), "x").unwrap();

        let names: Vec<_> = root
            .list_dir(Path::new("/tmp"))
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert!(names.contains(&OsString::from("pathshim-only")));
    }

    #[test]
    fn bind_projects_only_its_destination_subtree() {
        let temp = TempDir::new();
        let source = temp.0.join("bind-upper");
        let destination = temp.0.join("guest-destination");
        let outside = temp.0.join("outside.txt");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("lower.txt"), "lower").unwrap();
        fs::write(&outside, "outside").unwrap();

        let mut root = RootView::open_with_binds(
            None,
            &[BindSpec {
                source: source.clone(),
                destination: destination.clone(),
            }],
        )
        .unwrap();
        let source = fs::canonicalize(source).unwrap();

        assert!(root.projects(&destination.join("lower.txt")));
        assert!(!root.projects(&outside));
        assert_eq!(
            root.resolve_read(&destination.join("lower.txt")).unwrap(),
            destination.join("lower.txt")
        );

        let upper = root
            .prepare_open(&destination.join("lower.txt"), libc_flags::O_WRONLY)
            .unwrap();
        assert_eq!(upper, source.join("lower.txt"));
        assert_eq!(fs::read_to_string(upper).unwrap(), "lower");
        assert_eq!(
            root.prepare_open(&outside, libc_flags::O_WRONLY).unwrap(),
            outside
        );
    }

    #[test]
    fn bind_to_slash_is_a_root_projection() {
        let temp = TempDir::new();
        let source = temp.0.join("root-bind");
        let root = RootView::open_with_binds(
            None,
            &[BindSpec {
                source: source.clone(),
                destination: PathBuf::from("/"),
            }],
        )
        .unwrap();
        let source = fs::canonicalize(source).unwrap();

        assert_eq!(root.root_upper(), Some(source.as_path()));
        assert!(root.projects(Path::new("/anywhere")));
    }

    #[test]
    fn nested_bind_wins_over_root_projection() {
        let temp = TempDir::new();
        let root_source = temp.0.join("root-upper");
        let bind_source = temp.0.join("bind-upper");
        let destination = temp.0.join("guest-destination");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("lower.txt"), "lower").unwrap();

        let mut root = RootView::open_with_binds(
            Some(&root_source),
            &[BindSpec {
                source: bind_source.clone(),
                destination: destination.clone(),
            }],
        )
        .unwrap();

        let nested = root
            .prepare_open(&destination.join("lower.txt"), libc_flags::O_WRONLY)
            .unwrap();
        assert_eq!(
            nested,
            fs::canonicalize(bind_source).unwrap().join("lower.txt")
        );

        let root_path = Path::new("/pathshim-root-projection-test");
        let root_write = root
            .prepare_open(root_path, libc_flags::O_CREAT | libc_flags::O_WRONLY)
            .unwrap();
        assert_eq!(
            root_write,
            fs::canonicalize(root_source)
                .unwrap()
                .join("pathshim-root-projection-test")
        );
        assert!(!root.same_projection(&destination.join("lower.txt"), root_path));
        assert_eq!(
            root.rename(&destination.join("lower.txt"), root_path)
                .unwrap_err()
                .raw_os_error(),
            Some(libc_errno::EXDEV)
        );
    }
}
