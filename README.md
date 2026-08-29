# pathshim

`pathshim` gives a command a best-effort copy-on-write view of `/` and collects its file changes under one rootfs directory:

```console
pathshim -r <path> <command> [args...]
pathshim --rootfs=<path> <command> [args...]
```

Within the supported filesystem operations:

- `<path>` is the writable upper layer and is presented as `/` where projection is supported.
- The host `/` is the read fallback. A path under rootfs takes precedence over the same host path.
- New and modified files are written under rootfs.
- Deleting a host-only path records a persistent whiteout under `<path>/.pathshim/`; it does not delete the host path.
- Executables and runtime libraries can still come from the host, so rootfs does not need to contain a complete filesystem tree.

`pathshim` is a best-effort COW chroot alternative built on seccomp user notification, without `ptrace` or mount namespaces. It is a collection tool, not a security boundary or an equivalent replacement for kernel `chroot` or PRoot.

## How it relates to chroot and PRoot

| Tool | How the root view is created | Practical boundary |
|---|---|---|
| `chroot` | The kernel changes a process's filesystem root. Container-style isolation commonly combines it with a mount namespace. | Kernel-enforced root view; requires the corresponding privilege. |
| PRoot | `ptrace` observes syscalls and translates paths between guest and host rootfs. | Broad userspace chroot emulation, with `ptrace` availability and tracing overhead. |
| pathshim | Seccomp user notification delegates selected filesystem syscalls to a supervisor, which applies host-read-fallback and COW writes to rootfs. | Deliberately incomplete projection that degrades instead of preventing command startup. |

## Example

```console
cargo build --release

./target/release/pathshim -r /var/lib/pathshim/session-1 /bin/sh -c '
  cat /etc/os-release
  mkdir /project
  cd /project
  echo hello > result.txt
'
```

The OS release is read from the host when rootfs does not override it. The result is stored at:

```text
/var/lib/pathshim/session-1/project/result.txt
```

The rootfs path does not need to exist before the command starts. pathshim creates it recursively when the parent path is writable. It does not know or create caller-specific directory layouts.

## Runtime adaptation

pathshim selects the strongest mode that the current node actually supports. It probes behavior instead of maintaining a distribution or kernel-version allowlist:

1. `cow-root` verifies the complete projection path before the command starts, including seccomp user notification, parent-to-child memory access, and file descriptor injection.
2. `cwd` is selected when COW projection is unavailable. The command starts in the physical rootfs directory; caller-provided environment settings remain unchanged.
3. `passthrough` is selected when rootfs itself cannot be prepared. The command starts with its original environment and working directory.

The selected mode and a concise degradation reason are written to stderr. Capability detection never depends on whether the host is Kylin, Debian, CentOS, or another distribution.

## Kubernetes and Linux requirements

The design targets an ordinary Kubernetes Pod. It does not request:

- a privileged container;
- additional Linux capabilities;
- a custom AppArmor or seccomp profile;
- `/dev/fuse`; or
- a private mount namespace.

Full `cow-root` mode requires Linux kernel 5.9 or newer, procfs mounted at `/proc`, and an existing Pod security policy that does not explicitly block seccomp user notification or parent-to-child `process_vm_readv`/`process_vm_writev`. Older kernels and restrictive policies automatically use a lower mode. Full mode has been exercised on Linux 5.15 and in an unprivileged Docker container with `no-new-privileges` and the default container security profile.

If the deployment can grant a private mount namespace, use a mature tool such as bubblewrap instead. It provides a more complete filesystem view than pathshim can provide without mount privileges.

## Current coverage

The Linux backend currently projects common operations used by shells and applications:

- opening and creating files, including copy-up before writes;
- stat and access checks;
- directory creation, removal, rename, and merged directory listing;
- symlink creation and reading;
- virtual `chdir`, relative path resolution, and `getcwd`;
- path-based truncate, ownership, permission, and timestamp updates; and
- signal forwarding from pathshim to the command process group.

The projection works below the language runtime, so it covers both dynamically linked programs and static Go binaries for the supported operations.

## Known limitations

- Filesystem syscall coverage is intentionally incomplete. Operations such as hard-link creation, device/FIFO creation, `io_uring`-based file access, and executing a binary that exists only under rootfs are not projected yet.
- `/dev`, `/proc`, and `/sys` retain host/container semantics and are passed through.
- An unsupported operation may observe or modify the host/container filesystem. Do not use pathshim to run untrusted code or to enforce a read-only lower layer.
- Kernel and security-profile behavior varies across Kubernetes runtimes. Run the included Linux E2E tests on the target node image before adopting pathshim.
- Runtime E2E coverage currently runs on Linux x86_64. The aarch64 target is compile-checked but still needs runtime CI coverage.

## Development

Run the platform-independent model tests and formatting checks locally:

```console
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Run `cargo test` on Linux to include the COW E2E cases. They cover host read fallback, upper writes, merged directories, persistent whiteouts, virtual cwd, metadata copy-up, a static Go command, and Unix signal forwarding.

## License

Apache-2.0
