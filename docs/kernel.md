# pathshim Kernel

## 内核职责

pathshim 内核把 caller 提供的路径映射整理成一个 guest 文件系统视图，探测当前运行环境能够支持的最强模式，再在该模式下启动 command。它负责文件收拢和维持 guest 路径语义，不负责提供安全隔离，也不感知 caller 的业务对象。

一句话描述主流程：`CLI 配置 → Projection 集合 → 能力探测与模式选择 → command 执行 → syscall 路径转换与 COW`。

## 核心概念

### Invocation

一次 pathshim 启动及其托管的 command 执行树构成一个 invocation。能力选择、文件系统视图和进程终态都以 invocation 为边界。

### Projection

Projection 把一个物理 `source` 映射到 guest `destination`。`source` 是收拢写入的 upper，原 guest 绝对路径是读取回退使用的 lower。

- `--rootfs SOURCE` 等价于 `--bind SOURCE:/`。
- `--bind SOURCE:DEST` 可以重复指定；路径同时命中多个 destination 时，最长前缀优先。
- `/dev`、`/proc` 和 `/sys` 保留 Pod 原有语义，不参与映射。
- 没有 root projection 时，所有 bind 之外的路径直接使用 host 路径。

### RootView

RootView 是一个 invocation 的 Projection 集合及其 whiteout 状态。Linux supervisor 持有同一个 RootView，为 command 及其后代的受支持 syscall 做路径决策，因此映射选择不能随着线程、子进程或请求到达顺序变化。

一个 source 同时只允许一个活跃 invocation 使用。command 执行树内的并发由同一个 RootView 处理；多个独立 invocation 并发共享 source 不在契约内。

### Collect mode

pathshim 通过现场行为探测选择模式，不维护发行版或内核版本名单。

| 模式 | 文件系统视图 | command 启动方式 |
|---|---|---|
| `cow-view` | seccomp user notification 承载 Projection、COW 和 whiteout | 在 guest cwd 中执行，保留虚拟 `PWD` |
| `cwd` | 仅把 root projection 物理目录作为工作目录，无法表达局部 bind | 在物理 rootfs/cwd 中执行 |
| `passthrough` | 不做路径转换 | 以原始参数、继承环境和 caller cwd 直接执行 |

## 运行流程

1. `cli` 解析 rootfs、bind、guest cwd 和 command。缺少文件系统视图或 command 属于无效请求，直接返回参数错误。
2. `RootView` 创建并规范化 source，校验 destination，按 destination 长度组织 Projection。
3. 主流程在合并后的 guest 视图中解析初始 cwd；目录缺失时尝试在 upper 创建，无法使用时回退 guest `/`。
4. Linux child 安装 seccomp user-notification filter，并把 listener fd 交给 parent。parent 启动 filesystem supervisor，双方在 command `exec` 前完成握手和真实路径探测。
5. 探测成功后进入 `cow-view`。受支持的 syscall 由 `dispatch` 解析 guest 路径，通过 RootView 选择真实路径，再由 supervisor 执行或向 child 注入 fd；未覆盖的 syscall 继续由内核按 Pod 原始文件系统执行。
6. command 执行前发现 COW 不可用时，有 root projection 则降级为 `cwd`；bind-only 或 rootfs 也无法准备时降级为 `passthrough`。
7. parent 等待 command 终态，停止 filesystem supervisor，并把结果返回 caller。

能力降级只发生在 command `exec` 之前。command 已经开始运行后，不能通过重启 command 来切换模式，否则会重复其副作用。

## 文件系统视图

### 稳定映射

以 `--bind /var/lib/run/output:/output` 为例，guest 路径 `/output/result.txt` 的 lower 是原始 `/output/result.txt`，upper 是 `/var/lib/run/output/result.txt`。

- 写操作在受支持时落到 upper。
- 读操作先查 upper；upper 不存在且没有 whiteout 时，才回退 lower。
- 第一次修改 lower 条目时先 copy-up，后续受支持的读取继续命中 upper。
- 删除仅存在于 lower 的条目时记录 whiteout，后续读取不能再次暴露 lower。
- command 与后代并发访问时共享同一映射，不能一会儿命中 upper、一会儿绕回 lower。

best effort 表示 syscall 覆盖可能不完整，不表示已支持的映射可以不确定。并发读写仍遵循普通文件系统可见性；稳定映射不额外提供事务或写入原子性。

### 路径操作

- 同一 Projection 内的 rename 由 upper 承载；跨 Projection 或映射内外的 rename 返回 `EXDEV`，避免以复制加删除伪装原子 rename。
- 目录读取合并 upper 与 lower，并应用 whiteout。
- command、后续 executable 和动态链接库可以继续来自 host，因此 rootfs 不需要包含完整系统目录。
- guest cwd 覆盖常见的 `chdir`、`fchdir`、`getcwd`、相对路径和 `/proc/self/cwd`。当前通知流无法把任意 `clone(CLONE_FS)` 的 flags 与 child pid 精确关联，因此非常规 clone 组合只做 best effort。

## 进程托管与终态

Linux `cow-view` 需要 parent 持有 seccomp listener 并监督 child，因此 command 会经过 pathshim fork/exec，而不是由最初的 pathshim 进程直接 exec。这个实现需要服从“进程语义透明”约束：包装层不应改变 caller 对进程组、信号和终态的判断。

当前 command 不创建新的进程组，而是继承 caller 拥有的 PGID。pathshim parent 捕获 `SIGTERM`、`SIGINT`、`SIGHUP` 和 `SIGQUIT` 后转发给直接 child；等待 child 时，普通退出返回原 exit code，signal 终止当前转换为 `128 + signal`。

因此当前实现只闭合了 PGID 继承和常见终止信号转发，还没有实现严格透明的完整信号模型。后续调整必须把信号目标、重复投递、不可捕获信号以及 signal 终态保真作为一个整体处理，不能把 pathshim 演化成隐式的 init 或进程组 owner。

## 能力边界

- pathshim 不是安全边界。未覆盖的 syscall 可能看到或修改 Pod 原始文件系统。
- 当前覆盖常见同步文件操作，不承诺覆盖 hard link、设备或 FIFO 创建、`io_uring` 文件访问，以及仅存在于 upper 的 executable 启动。
- seccomp user notification、进程内存访问或 fd 注入不可用时按启动降级规则处理，不要求 caller 修改 Pod 权限。
- 运行日志必须给出实际 collect mode 和降级原因，caller 不能仅根据配置推断最终能力。

## 实现入口

- [`src/main.rs`](../src/main.rs)：组装 invocation、解析 guest cwd、选择降级路径并最终 exec。
- [`src/root.rs`](../src/root.rs)：Projection/RootView、copy-up、whiteout、目录合并和路径选择。
- [`src/linux/mod.rs`](../src/linux/mod.rs)：child、listener、supervisor、能力探测和进程终态。
- [`src/linux/dispatch.rs`](../src/linux/dispatch.rs)：受支持 syscall 的 guest 语义。
- [`src/linux/seccomp.rs`](../src/linux/seccomp.rs)：通知 filter 与内核接口。
- [`src/linux/remote.rs`](../src/linux/remote.rs)：读取 tracee 参数、cwd 和 fd 状态。
