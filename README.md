# pathshim

`pathshim` gives a command a best-effort copy-on-write view of `/` and collects its file changes under one rootfs directory:

```console
pathshim -r <path> <command> [args...]
pathshim --rootfs=<path> <command> [args...]
pathshim -r <path> --cwd <guest-path> <command> [args...]
```

Within the supported filesystem operations:

- `<path>` is the writable upper layer and is presented as `/` where projection is supported.
- The host `/` is the read fallback. A path under rootfs takes precedence over the same host path.
- New and modified files are written under rootfs.
- Deleting a host-only path records a persistent whiteout under `<path>/.pathshim/`; it does not delete the host path.
- Executables and runtime libraries can still come from the host, so rootfs does not need to contain a complete filesystem tree.

`pathshim` pursues the same product goal as PRoot: give an unprivileged process a guest root filesystem view. It uses seccomp user notification instead of `ptrace` or mount namespaces and deliberately accepts degraded, best-effort coverage. It is a collection tool, not a security boundary or a feature-equivalent replacement for kernel `chroot` or PRoot.

## How it relates to chroot and PRoot

| Tool | How the root view is created | Practical boundary |
|---|---|---|
| `chroot` | The kernel changes a process's filesystem root. Container-style isolation commonly combines it with a mount namespace. | Kernel-enforced root view; requires the corresponding privilege. |
| PRoot | `ptrace` observes syscalls and translates paths between guest and host rootfs. | Broad userspace chroot emulation, with `ptrace` availability and tracing overhead. |
| pathshim | Seccomp user notification delegates selected filesystem syscalls to a supervisor, which applies host-read-fallback and COW writes to rootfs. | Deliberately incomplete projection that degrades instead of preventing command startup. |

Use `-w`, `--cwd`, or `--pwd` to select the initial working directory inside the guest view. The default is `/`. The path is normalized and checked against the merged upper/lower view. A missing directory is created in the writable upper; a non-directory path or creation failure emits a warning and falls back to `/`.

## Example

```console
cargo build --release
mkdir -p /var/lib/pathshim/session-1/project

./target/release/pathshim -r /var/lib/pathshim/session-1 -w /project /bin/sh -c '
  cat /etc/os-release
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
- guest `chdir`, `fchdir`, relative path resolution, `getcwd`, and `/proc/self/cwd`;
- the common cwd inheritance model: pthreads share cwd state, while a forked process receives independent state when it changes cwd;
- path-based truncate, ownership, permission, and timestamp updates; and
- caller process-group inheritance and direct signal forwarding from pathshim to the command.

The projection works below the language runtime, so it covers both dynamically linked programs and static Go binaries for the supported operations.

## Known limitations

- Filesystem syscall coverage is intentionally incomplete. Operations such as hard-link creation, device/FIFO creation, `io_uring`-based file access, and executing a binary that exists only under rootfs are not projected yet.
- `/dev`, `/proc`, and `/sys` retain host/container semantics and are passed through.
- Self cwd links are projected, but the rest of `/proc` keeps host/container semantics.
- The current filesystem notification stream has no clone lifecycle event that correlates clone flags with the new child pid. Exact sharing for arbitrary `clone(CLONE_FS)` users and an exact fork-time cwd snapshot are best effort; ordinary pthread and fork flows are covered.
- An unsupported operation may observe or modify the host/container filesystem. Do not use pathshim to run untrusted code or to enforce a read-only lower layer.
- Kernel and security-profile behavior varies across Kubernetes runtimes. Run the included Linux E2E tests on the target node image before adopting pathshim.
- Automated runtime E2E coverage currently runs on Linux x86_64. The Linux E2E suite and Docker smoke test have also been exercised manually on aarch64, but aarch64 still needs automated runtime CI coverage.

## Development

Run the platform-independent model tests and formatting checks locally:

```console
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Run `cargo test` on Linux to include the COW E2E cases. They cover host read fallback, upper writes, merged directories, persistent whiteouts, guest cwd and PWD, `chdir`/`fchdir`/`getcwd`, `/proc/self/cwd`, pthread/fork cwd behavior, metadata copy-up, a static Go command, and Unix signal forwarding.

Run the Docker smoke test on each target architecture to verify `cow-root` inside an unprivileged container using Docker's default seccomp profile, no added capabilities, a non-root user, and `no-new-privileges`:

```console
./tests/docker_smoke.sh
```

The smoke test requires Docker, Go, and a Rust toolchain. It builds static pathshim and Go fixture binaries, imports a temporary scratch image, verifies that the fixture's write is collected under rootfs, and removes its image and temporary files when finished.

## License

Apache-2.0
