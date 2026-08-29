# AGENTS.md

## 项目定位与边界

pathshim 是运行在普通 Kubernetes Pod 内的 best-effort 文件系统视图工具。它不得要求 Pod 具备 `privileged`、额外 capability、自定义 seccomp/AppArmor、`/dev/fuse` 或其它集群侧特殊照顾；如果部署环境能够提供 mount namespace 等能力，应直接选择 bubblewrap，而不是让 pathshim 重复实现特权方案。

pathshim 是独立的通用项目，通过根映射或局部 bind 把进程运行产物尽量收拢到指定目录，不绑定任何上层业务概念。其产品目标与 PRoot 一致：为非特权进程提供 guest 文件系统视图；区别是 pathshim 接受能力降级与 best-effort 覆盖，不追求 PRoot 基于 ptrace 的完整 syscall 仿真。它不是安全边界，也不承诺等价于 `chroot`、mount namespace 或容器文件系统。

## 核心契约

稳定入口是：

```console
pathshim -r <path> [--] <command> [args...]
pathshim --rootfs=<path> [--] <command> [args...]
pathshim -b <source:guest-destination> [--] <command> [args...]
pathshim --bind=<source:guest-destination> [--] <command> [args...]
pathshim -r <path> --cwd <guest-path> [--] <command> [args...]
```

无论 pathshim 从哪个目录启动，都应尽可能让 command 及其子进程把 `<path>` 视为 `/`。由于普通 Pod 内没有 mount namespace，路径视图允许不完整，但必须遵循以下方向：

- `--rootfs SOURCE` 是 `--bind SOURCE:/` 的便捷入口；`--bind SOURCE:DEST` 是 guest `DEST` 子树的可写 upper，可以重复指定，目标路径最长匹配优先。
- 读取路径时优先使用命中映射的 source；source 不存在对应文件时回退到原 guest 绝对路径。新建和修改的文件应尽可能写入 source，而不是修改 lower。
- 没有配置 `--rootfs` 时，所有 bind 之外的路径均透传宿主。跨映射或映射内外的 rename 返回 `EXDEV`，不得以复制加删除破坏原子性。
- 同一次 pathshim 执行树内，command 及其所有后代必须共享同一个确定的 guest 文件系统视图：相同路径稳定命中同一映射，写入与 whiteout 对后续 syscall 立即可见，不得因线程、子进程或 syscall 类型改变读写落点。一个 source 同时只由一个活跃 pathshim invocation 持有；独立 invocation 并发共享 source 不在契约内。
- command 本身、后续启动的可执行文件及其运行时依赖可以继续从宿主全局路径加载，因此 rootfs 不需要包含完整文件系统树。
- `--cwd`、`--pwd` 或 `-w` 指定 guest 视图中的初始工作目录，默认 `/`；目录缺失时在 upper 中创建，路径不是目录或创建失败时回退 `/`，不得把 upper 的物理路径暴露为 guest cwd。该行为有意区别于 PRoot 的缺失即回退，更符合 pathshim 收拢运行产物的职责。
- cwd 状态应覆盖常见的 `chdir`、`fchdir`、`getcwd`、相对路径、`/proc/self/cwd`、pthread 共享和 fork 后独立修改。当前 filesystem 通知流没有可关联 clone flags 与 child pid 的生命周期事件，因此任意 `clone(CLONE_FS)` 的精确共享与 fork 时刻快照只做 best effort。
- 无法覆盖的 syscall、运行时或路径场景必须被视为能力缺口；不得把 best-effort 行为描述成完整隔离。
- command 可启动性优先于文件收拢能力。合法请求在 command 启动前发现内核、策略或运行时能力不可用时，pathshim 必须逐级降级：根映射可先降级 `cwd`，bind-only 直接降级 `passthrough`；末级以调用者原始 command、参数、继承环境和工作目录执行，不得因收拢能力缺失拒绝启动或依赖发行版/内核版本名单。source 不存在时由 pathshim 创建。
- pathshim 只认识 source 与 guest destination 的路径映射，不认识任何 caller 业务概念，也不应主动创建或改写 caller 的目录布局与环境约定。

以 `--bind /var/lib/run/output:/output` 为例，guest 路径 `/output/result.txt` 的原始 host 路径仍是 `/output/result.txt`，收拢后的 upper 路径是 `/var/lib/run/output/result.txt`：

- 受支持的写操作必须落到 upper；读操作先查 upper，不存在且没有 whiteout 时才回退原始 host 路径。
- 一旦写入或 copy-up 到 upper，后续受支持的读取必须继续命中 upper；删除仅存在于 lower 的条目时记录 whiteout，不能再次暴露 lower。
- command 及其后代并发访问时必须共享这一映射，不能一会儿命中 upper、一会儿绕回原始路径。
- best effort 只表示部分 syscall 可能无法覆盖，不表示已经支持的映射可以不稳定。独立 pathshim invocation 并发共享同一 source 不在保证范围内。

## 开发约定

- 不把网络、进程、权限限制等 sandbox 策略带入 pathshim；文件收拢是唯一职责。
- CLI 与调用契约只表达 source 对应 guest destination 的语义，不把 seccomp、ptrace、FUSE 等机制变成参数或上层概念；README 可以用机制对比解释能力边界和现场要求。
- 运行日志应说明实际启用的收拢能力和降级原因，避免调用方误判覆盖范围。
- 文件系统行为测试至少覆盖动态链接程序和静态 Go 程序，并验证根映射与 bind 的读取回退、写入 upper、映射外透传、子进程继承、guest cwd 和重复执行后的持久性；cwd 场景按 PRoot 的测试思路拆成独立 fixture 与集成测试。
