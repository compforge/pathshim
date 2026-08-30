#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct BindSpec {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
}

#[derive(Debug)]
// A bind view is deliberately stateless: every supported operation below a
// destination goes directly to its source. Independent invocations may share a
// source; their visibility and write races are ordinary filesystem semantics.
pub(crate) struct BindView {
    projections: Vec<Projection>,
}

#[derive(Debug)]
struct Projection {
    source: PathBuf,
    destination: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct DirEntry {
    pub(crate) name: OsString,
    pub(crate) inode: u64,
    pub(crate) file_type: u8,
}

impl BindView {
    pub(crate) fn open(binds: &[BindSpec]) -> io::Result<Self> {
        let mut destinations = HashSet::new();
        let mut projections = Vec::with_capacity(binds.len());
        for bind in binds {
            if !bind.destination.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "bind destination must be absolute: {}",
                        bind.destination.display()
                    ),
                ));
            }
            let destination = normalize_virtual(&bind.destination)?;
            if destination == Path::new("/") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bind destination `/` is unsupported",
                ));
            }
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
            fs::create_dir_all(&bind.source)?;
            projections.push(Projection {
                source: fs::canonicalize(&bind.source)?,
                destination,
            });
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
        Self::open(&binds)
    }

    pub(crate) fn projection_count(&self) -> usize {
        self.projections.len()
    }

    pub(crate) fn probe_path(&self) -> &Path {
        &self
            .projections
            .first()
            .expect("a bind view has at least one projection")
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
        self.resolve_projected(virtual_path)
    }

    pub(crate) fn resolve_directory(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let real_path = self.resolve_projected(&virtual_path)?;
        match real_path.metadata() {
            Ok(metadata) if metadata.is_dir() => Ok(virtual_path),
            Ok(_) => Err(io::Error::from_raw_os_error(libc_errno::ENOTDIR)),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn create_directory_all(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        if self.projection_index(&virtual_path).is_none() {
            return Err(io::Error::from_raw_os_error(libc_errno::ENOTSUP));
        }
        let real_path = self.resolve_projected(&virtual_path)?;
        fs::create_dir_all(real_path)?;
        Ok(virtual_path)
    }

    pub(crate) fn prepare_open(&self, virtual_path: &Path, _flags: i32) -> io::Result<PathBuf> {
        self.resolve_projected(virtual_path)
    }

    pub(crate) fn prepare_mutation(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        self.resolve_projected(virtual_path)
    }

    pub(crate) fn mkdir(&self, virtual_path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new()
            .mode(mode)
            .create(self.resolve_projected(virtual_path)?)
    }

    pub(crate) fn unlink(&self, virtual_path: &Path, remove_dir: bool) -> io::Result<()> {
        let real_path = self.resolve_projected(virtual_path)?;
        if remove_dir {
            fs::remove_dir(real_path)
        } else {
            fs::remove_file(real_path)
        }
    }

    pub(crate) fn rename(&self, old: &Path, new: &Path) -> io::Result<()> {
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
        fs::rename(
            self.projections[old_index].resolve(&old),
            self.projections[new_index].resolve(&new),
        )
    }

    pub(crate) fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, self.resolve_projected(link)?)
    }

    pub(crate) fn list_dir(&self, virtual_path: &Path) -> io::Result<Vec<DirEntry>> {
        use std::os::unix::fs::MetadataExt;

        let mut entries = Vec::new();
        for entry in fs::read_dir(self.resolve_projected(virtual_path)?)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            entries.push(DirEntry {
                name: entry.file_name(),
                inode: metadata.ino(),
                file_type: metadata_to_dir_type(&metadata),
            });
        }
        Ok(entries)
    }

    pub(crate) fn virtual_for_host(&self, host: &Path) -> PathBuf {
        self.projections
            .iter()
            .filter_map(|projection| {
                host.strip_prefix(&projection.source).ok().map(|relative| {
                    (
                        projection.source.components().count(),
                        join_virtual(&projection.destination, relative),
                    )
                })
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, virtual_path)| virtual_path)
            .unwrap_or_else(|| host.to_path_buf())
    }

    fn resolve_projected(&self, virtual_path: &Path) -> io::Result<PathBuf> {
        let virtual_path = normalize_virtual(virtual_path)?;
        let Some(index) = self.projection_index(&virtual_path) else {
            return Ok(virtual_path);
        };
        Ok(self.projections[index].resolve(&virtual_path))
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
}

impl Projection {
    fn resolve(&self, virtual_path: &Path) -> PathBuf {
        self.source.join(
            virtual_path
                .strip_prefix(&self.destination)
                .expect("projection selected by destination prefix"),
        )
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

fn join_virtual(destination: &Path, relative: &Path) -> PathBuf {
    if relative.as_os_str().is_empty() {
        destination.to_path_buf()
    } else {
        destination.join(relative)
    }
}

fn is_reserved(path: &Path) -> bool {
    ["/dev", "/proc", "/sys"]
        .iter()
        .any(|prefix| path == Path::new(prefix) || path.starts_with(prefix))
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

mod libc_errno {
    pub(super) const EXDEV: i32 = 18;
    pub(super) const ENOTDIR: i32 = 20;
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
                .join(format!("pathshim-bind-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test directory");
        }
    }

    fn view(source: &Path, destination: &str) -> BindView {
        BindView::open(&[BindSpec {
            source: source.to_path_buf(),
            destination: PathBuf::from(destination),
        }])
        .unwrap()
    }

    #[test]
    fn bind_replaces_destination_without_lower_fallback() {
        let temp = TempDir::new();
        let source = temp.0.join("source");
        let view = view(&source, "/workspace");

        assert_eq!(
            view.resolve_read(Path::new("/workspace/missing.txt"))
                .unwrap(),
            fs::canonicalize(&source).unwrap().join("missing.txt")
        );
        assert!(!source.join(".pathshim").exists());
    }

    #[test]
    fn independent_views_observe_external_writes() {
        let temp = TempDir::new();
        let source = temp.0.join("source");
        let first = view(&source, "/workspace");
        let second = first.reopen().unwrap();

        fs::write(source.join("result.txt"), "shared").unwrap();

        assert_eq!(
            fs::read_to_string(
                second
                    .resolve_read(Path::new("/workspace/result.txt"))
                    .unwrap()
            )
            .unwrap(),
            "shared"
        );
    }

    #[test]
    fn longest_destination_and_source_prefix_win() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let cache = workspace.join("cache");
        let view = BindView::open(&[
            BindSpec {
                source: workspace.clone(),
                destination: PathBuf::from("/workspace"),
            },
            BindSpec {
                source: cache.clone(),
                destination: PathBuf::from("/workspace/cache"),
            },
        ])
        .unwrap();

        assert_eq!(
            view.resolve_read(Path::new("/workspace/cache/item"))
                .unwrap(),
            fs::canonicalize(&cache).unwrap().join("item")
        );
        assert_eq!(
            view.virtual_for_host(&fs::canonicalize(&cache).unwrap().join("item")),
            PathBuf::from("/workspace/cache/item")
        );
    }

    #[test]
    fn creates_missing_guest_cwd_inside_bind() {
        let temp = TempDir::new();
        let source = temp.0.join("source");
        let view = view(&source, "/workspace");

        assert_eq!(
            view.create_directory_all(Path::new("/workspace/project"))
                .unwrap(),
            PathBuf::from("/workspace/project")
        );
        assert!(source.join("project").is_dir());
    }

    #[test]
    fn rejects_root_and_reserved_destinations() {
        let temp = TempDir::new();
        for destination in ["/", "/proc/guest"] {
            let error = BindView::open(&[BindSpec {
                source: temp.0.join(destination.trim_start_matches('/')),
                destination: PathBuf::from(destination),
            }])
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
