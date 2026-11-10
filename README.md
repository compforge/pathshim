# pathshim

`pathshim` gives a command best-effort bind path mappings without mount privileges:

```console
pathshim --bind <source:guest-destination> [--cwd <guest-path>] -- <command> [args...]
```

It is useful when several processes need a stable path such as `/workspace`, while their real data lives elsewhere:

```console
mkdir -p /var/lib/session-1/workspace
pathshim \
  --bind /var/lib/session-1/workspace:/workspace \
  --cwd /workspace \
  -- /bin/sh
```

Within supported filesystem operations, `/workspace/result.txt` now refers to `/var/lib/session-1/workspace/result.txt`.

## Core behavior

- A bind is a replace mapping: paths below guest `DEST` resolve only below physical `SOURCE`. The Pod's original `DEST` tree is not a read fallback.
- Binds may be repeated. When destinations overlap, the longest matching destination wins.
- Paths outside every bind retain their normal Pod filesystem semantics.
- Sources must already be directories searchable by the invocation's effective identity, including every ancestor. The caller prepares their ownership, permissions and lifecycle; pathshim never creates or repairs them.
- `DEST` must be absolute, below `/`, and outside `/dev`, `/proc`, and `/sys`.
- A bind view stores no metadata. Independent pathshim invocations and external writers may share a source concurrently; visibility and write races follow the backing filesystem.

`pathshim` is not a security boundary or a complete bind mount emulator. Unsupported syscalls may bypass the mapping and operate on the Pod's original path.

## Guest working directory

Use `-w`, `--cwd`, or `--pwd` to choose the initial guest cwd. The default is `/`.

The requested cwd must already exist, be a directory and be searchable by the invocation's effective identity. A missing, non-directory or inaccessible cwd fails before command startup; pathshim neither creates it nor substitutes `/`.

```console
mkdir -p /var/lib/session-1/workspace/project
pathshim \
  --bind /var/lib/session-1/workspace:/workspace \
  --cwd /workspace/project \
  -- /bin/sh -c 'pwd; echo hello > result.txt'
```

The command sees `/workspace/project`; the output is stored at `/var/lib/session-1/workspace/project/result.txt`.

## Runtime adaptation

pathshim probes the actual node instead of maintaining a kernel or distribution allowlist:

1. `bind-view` verifies seccomp user notification, parent-to-child memory access, file descriptor injection, and every configured destination plus the requested cwd before command startup.
2. `passthrough` runs the original command and arguments with inherited environment and caller cwd when bind-view cannot be installed.

The selected mode and degradation reason are written to stderr. Integrations that keep control-plane diagnostics separate from command output can use `--quiet`.

For valid requests, command startup takes priority over mechanism availability. Invalid mappings or source/cwd access errors exit `2` without running the command, including under `--quiet`. Source and cwd errors identify the stage, physical source, guest destination and underlying errno. This validation is read-only and is not a promise that every future file operation will be permitted. Once a command has started, pathshim never restarts it in another mode because doing so could repeat side effects.

### Startup probe

Long-running callers can test the exact bind configuration during startup without running a user command:

```console
mkdir -p /var/lib/session-probe/workspace
pathshim probe --bind /var/lib/session-probe/workspace:/workspace --cwd /workspace
```

The probe performs the same seccomp listener handshake and live Projection checks used before a normal command. It prints `bind-view` and exits `0` when mapping is available; it prints `passthrough` and exits `1` when a normal invocation would degrade, with the reason on stderr. Invalid CLI input, mappings, sources or cwd exit `2` without a mode on stdout. Internal probe errors also fail without claiming passthrough. Run the probe under the same UID, GID, groups and security policy as the intended invocation; a root probe does not establish access for another UID. Directory search permission is required, not directory read/write permission. The caller owns the probe sources and may remove them afterward.

### Compatibility in 0.1.7

Older versions created missing sources and guest cwd paths, substituted `/` for invalid cwd,
and could run passthrough after a mapping preparation error. Callers must now prepare sources
and cwd explicitly. Mechanism-unavailable fallback and command exit/signal forwarding remain.

## Kubernetes and Linux requirements

pathshim targets an ordinary Kubernetes Pod. It does not request:

- a privileged container;
- additional Linux capabilities;
- a custom AppArmor or seccomp profile;
- `/dev/fuse`; or
- a private mount namespace.

Full `bind-view` mode requires Linux kernel 5.9 or newer, procfs mounted at `/proc`, and a security profile that does not block seccomp user notification or parent-to-child `process_vm_readv`/`process_vm_writev`. Older kernels and restrictive startup policies automatically use passthrough. Rewriting an executable pathname stored in read-only memory additionally uses the same parent-child access through `/proc/<pid>/mem`; if only that operation is blocked, the mapped exec fails without disabling the rest of the active bind view.

Full mode has been exercised on Linux 5.15 and in an unprivileged Docker container with `no-new-privileges` and the default container security profile. If a deployment can grant a private mount namespace and needs complete filesystem semantics, use bubblewrap instead.

## Current coverage

The Linux backend projects common operations used by shells and applications:

- opening and creating files;
- stat and access checks;
- directory creation, removal, rename, and listing;
- symlink creation and reading;
- guest `chdir`, `fchdir`, relative paths, `getcwd`, and `/proc/self/cwd`;
- executing readable ELF binaries through mapped absolute paths with `execve` and `execveat`;
- path-based truncate, ownership, permission, and timestamp updates; and
- caller process-group inheritance, common termination signal forwarding, and signal terminal-status preservation.

The mapping works below language runtimes, so supported operations cover dynamically linked programs and static Go binaries.

## Known limitations

- Filesystem syscall coverage is incomplete. Hard links, device/FIFO creation, `io_uring` file access, and some extended-attribute operations are not mapped yet.
- Mapped absolute execution currently targets readable ELF binaries. Execute-only files cannot be injected, and shebang scripts should be invoked through their interpreter.
- The guest executable pathname must be long enough for pathshim's internal `/dev/fd/<n>` alias. Normal `/workspace/...` paths satisfy this in ordinary process FD ranges.
- `/dev`, `/proc`, and `/sys` retain Pod semantics and cannot be bind destinations.
- Rename across projections, or between mapped and unmapped paths, returns `EXDEV`. Nonzero `renameat2` flags are not supported yet.
- Exact sharing for arbitrary `clone(CLONE_FS)` combinations and exact fork-time cwd snapshots remain best effort.
- Unsupported operations may observe or modify the Pod filesystem. Do not use pathshim to run untrusted code or enforce access control.

## Development

Run platform-independent validation locally:

```console
make format
make lint
make test
```

`make format` applies Rust formatting; `make lint` checks formatting and runs Clippy with warnings denied. `make test` runs Cargo tests with the checked-in lockfile. These targets are also the entrypoints for `repocli validate`.

On Linux, `make test` also runs E2E coverage for replace semantics, mapping-external passthrough, multiple independent invocations sharing one source, external writers, guest cwd, pthread/fork cwd behavior, static Go build plus mapped `execve`/`execveat`, an installed Python runtime and child process, quiet diagnostics, and signal behavior. A skipped optional runtime is not verified coverage.

Run the Docker smoke test on each target architecture to verify `bind-view` and `passthrough` inside an unprivileged container:

```console
./tests/docker_smoke.sh
```

Use an existing architecture-compatible image containing `ffmpeg` and `ffprobe` for the optional real-command smoke test:

```console
PATHSHIM_FFMPEG_IMAGE=<image> ./tests/ffmpeg_smoke.sh
```

Run the capability audit to report known best-effort bypasses on a target Linux node:

```console
./tests/capability_audit.sh
```

## License

Apache-2.0
