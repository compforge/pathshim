# pathshim Kernel

## 内核职责

pathshim 内核把 caller 提供的物理目录映射成 command 使用的 guest 子树，探测当前环境是否能承载这组映射，再在选定模式下启动 command。它负责 best-effort 路径转换，不负责 rootfs、COW、安全隔离或进程治理。

一句话描述主流程：`CLI bind 配置 → BindView → 能力探测与模式选择 → command 执行 → syscall 路径转换`。

## 核心概念

### Invocation

一次 pathshim 启动及其托管的 command 执行树构成一个 invocation。能力选择、进程组继承和终态都以 invocation 为边界。

### Projection

Projection 把一个物理 `source` 映射到 guest `destination`。它是 replace 映射：destination 内受支持的操作只访问 source；destination 外的路径保留 Pod 语义。

- `--bind SOURCE:DEST` 可以重复指定；路径命中多个 destination 时最长前缀优先。
- destination 必须是 `/` 以下的绝对路径，且不能位于 `/dev`、`/proc`、`/sys`。
- 同一 Projection 内的 rename 由 source 承载；跨 Projection 或映射内外的 rename 返回 `EXDEV`。

### BindView

BindView 是一个 invocation 使用的 Projection 集合，负责 guest → physical 和 physical → guest 双向选择。它不缓存文件存在性，不保存 whiteout 或其它持久状态。

多个独立 invocation 及 file API 等外部 writer 可以共享同一个 source。pathshim 不增加全局锁或事务：不同文件并发、同文件写冲突、`O_EXCL` 和 rename 原子性都遵循底层文件系统。

### Collect mode

pathshim 基于现场行为探测选择模式，不维护发行版或内核版本名单。

| 模式 | 文件系统视图 | command 启动方式 |
|---|---|---|
| `bind-view` | seccomp user notification 承载受支持的 bind 路径操作 | 在 guest cwd 中执行，保留虚拟 `PWD` |
| `passthrough` | 不做路径转换 | 以原始参数、继承环境和 caller cwd 执行 |

## 运行流程

1. `cli` 解析 repeatable bind、guest cwd、quiet 和 command；缺少 bind 或 command 属于参数错误。
2. BindView 创建 source、规范化 destination、拒绝重复或保留路径，并按 destination 长度排列 Projection。
3. 主流程解析初始 cwd；映射内目录缺失时在 source 创建，无法使用时回退 guest `/`。
4. Linux child 安装 seccomp user-notification filter，把 listener fd 交给 parent；parent 启动 filesystem supervisor，双方在 command `exec` 前完成握手和真实映射探测。
5. 探测成功后进入 `bind-view`。受支持 filesystem syscall 由 `dispatch` 解析 guest 路径；`execve`/`execveat` 由 `execute` 打开 source executable、向 child 注入 close-on-exec fd，并通过短 `/dev/fd` 路径继续原 syscall。
6. command 执行前发现映射不可用时进入 `passthrough`，重建原 command 以清除 guest cwd 和 `PWD`。
7. parent 等待 command 终态，停止 filesystem supervisor，并把 exit code 或 signal 原样返回 caller。

模式选择只发生在 command `exec` 之前；command 启动后不能通过重启切换模式，否则可能重复副作用。

## 路径与并发

以 `--bind /var/lib/run/workspace:/workspace` 为例：

```text
/workspace/result.txt               guest
        ↓
/var/lib/run/workspace/result.txt   physical
```

- `/workspace` 原有的 Pod 目录不会参与读取或目录合并。
- `/tmp/result.txt` 等映射外路径直接使用 Pod 文件系统。
- command 后代继承同一个 seccomp filter；各自 invocation 的 supervisor 根据同一无状态规则解析路径。
- 外部 writer 直接写 source 后，活跃 invocation 的下一次受支持操作立即按底层文件系统观察结果。
- descriptor-relative 路径和真实 cwd 通过 source 最长前缀反向映射回 destination。

best effort 只限制 syscall 覆盖，不允许已覆盖操作因为 invocation 数量而改变落点。

## 进程托管与终态

`bind-view` 需要 parent 持有 seccomp listener 并监督 child，因此 command 会经过 pathshim fork/exec。pathshim 不创建新进程组；command 继承 caller PGID，Hostel 等上层 supervisor 仍然拥有整个 execution process group。

pathshim 捕获 `SIGTERM`、`SIGINT`、`SIGHUP` 和 `SIGQUIT` 后转发给直接 child。等待结果时，普通退出沿用 exit code；signal 终止则恢复默认 handler 并以同一 signal 终止 pathshim，自然区分 `exit(143)` 与 `SIGTERM`。

pathshim 不充当 PID 1、subreaper 或 tini。不可捕获信号和整棵进程树的强制清理由 caller 的进程组策略负责。

## 能力边界

- pathshim 不是安全边界；未覆盖 syscall 可能访问 Pod 原始 destination。
- 当前不完整覆盖 hard link、设备/FIFO、extended attributes 和 `io_uring` 文件访问。映射后的绝对执行当前面向 readable ELF；execute-only 文件与 shebang 脚本仍是 best effort 边界。
- seccomp user notification、进程内存访问或 fd 注入不可用时进入 passthrough，不要求 caller 修改 Pod 权限。
- `--quiet` 只抑制能力和降级诊断，不能吞掉无效参数或无法执行 command 等调用错误。
- guest cwd 覆盖常见 pthread/fork；任意 `clone(CLONE_FS)` 组合仍是 best effort。

## 实现入口

- [`src/main.rs`](../src/main.rs)：组装 invocation、解析 guest cwd、选择 passthrough 并最终 exec。
- [`src/bind.rs`](../src/bind.rs)：Projection、BindView 与双向路径选择。
- [`src/linux/mod.rs`](../src/linux/mod.rs)：child、listener、supervisor、能力探测和进程终态。
- [`src/linux/dispatch.rs`](../src/linux/dispatch.rs)：受支持 syscall 的 guest 语义。
- [`src/linux/execute.rs`](../src/linux/execute.rs)：映射 `execve`/`execveat` 的 executable fd 与 pathname。
- [`src/linux/seccomp.rs`](../src/linux/seccomp.rs)：通知 filter 与内核接口。
- [`src/linux/remote.rs`](../src/linux/remote.rs)：读取 tracee 参数、cwd 和 fd 状态。
