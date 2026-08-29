# AGENTS.md

## 项目定位与边界

pathshim 是运行在普通 Kubernetes Pod 内的 best-effort 文件系统视图工具。它不得要求 Pod 具备 `privileged`、额外 capability、自定义 seccomp/AppArmor、`/dev/fuse` 或其它集群侧特殊照顾；如果部署环境能够提供 mount namespace 等能力，应直接选择 bubblewrap，而不是让 pathshim 重复实现特权方案。

pathshim 是独立的通用项目，只帮助进程把运行产物尽量收拢到 `--rootfs` 指定目录，不绑定任何上层业务概念。其产品目标与 PRoot 一致：为非特权进程提供 guest rootfs 视图；区别是 pathshim 接受能力降级与 best-effort 覆盖，不追求 PRoot 基于 ptrace 的完整 syscall 仿真。它不是安全边界，也不承诺等价于 `chroot`、mount namespace 或容器文件系统。

## 核心契约

稳定入口是：

```console
pathshim -r <path> [--] <command> [args...]
pathshim --rootfs=<path> [--] <command> [args...]
pathshim -r <path> --cwd <guest-path> [--] <command> [args...]
```

无论 pathshim 从哪个目录启动，都应尽可能让 command 及其子进程把 `<path>` 视为 `/`。由于普通 Pod 内没有 mount namespace，路径视图允许不完整，但必须遵循以下方向：

- `--rootfs` 是可写 upper，宿主文件系统是只读语义的 lower。
- 读取路径时优先使用 rootfs 中的文件；rootfs 不存在对应文件时回退到宿主全局路径。
- 新建和修改的文件应尽可能写入 rootfs，而不是修改宿主 lower。
- command 本身、后续启动的可执行文件及其运行时依赖可以继续从宿主全局路径加载，因此 rootfs 不需要包含完整文件系统树。
- `--cwd`、`--pwd` 或 `-w` 指定 guest 视图中的初始工作目录，默认 `/`；目录无效时回退 `/`，不得把 upper 的物理路径暴露为 guest cwd。
- cwd 状态应覆盖常见的 `chdir`、`fchdir`、`getcwd`、相对路径、`/proc/self/cwd`、pthread 共享和 fork 后独立修改。当前 filesystem 通知流没有可关联 clone flags 与 child pid 的生命周期事件，因此任意 `clone(CLONE_FS)` 的精确共享与 fork 时刻快照只做 best effort。
- 无法覆盖的 syscall、运行时或路径场景必须被视为能力缺口；不得把 best-effort 行为描述成完整隔离。
- command 可启动性优先于文件收拢能力。pathshim 应基于现场行为探测依次选择 `cow-root`、`cwd`、`passthrough`，不得依赖发行版或内核版本名单；rootfs 不存在时由 pathshim 创建。
- pathshim 只认识 rootfs 对应虚拟 `/`，不认识任何 caller 业务概念，也不应主动创建或改写 caller 的目录布局与环境约定。

## 开发约定

- 不把网络、进程、权限限制等 sandbox 策略带入 pathshim；文件收拢是唯一职责。
- CLI 与调用契约只表达 rootfs 对应虚拟 `/` 的语义，不把 seccomp、ptrace、FUSE 等机制变成参数或上层概念；README 可以用机制对比解释能力边界和现场要求。
- 运行日志应说明实际启用的收拢能力和降级原因，避免调用方误判覆盖范围。
- 文件系统行为测试至少覆盖动态链接程序和静态 Go 程序，并验证读取回退、写入 upper、子进程继承、guest cwd 和重复执行后的持久性；cwd 场景按 PRoot 的测试思路拆成独立 fixture 与集成测试。
