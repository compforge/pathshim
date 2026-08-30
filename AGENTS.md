# AGENTS.md

## 项目定位与边界

pathshim 是运行在普通 Kubernetes Pod 内的 best-effort bind path mapper。它把 caller 提供的物理 source 映射到 command 看到的 guest destination，帮助多个进程把选定目录当成稳定路径使用；它不得要求 Pod 具备 `privileged`、额外 capability、自定义 seccomp/AppArmor、`/dev/fuse` 或其它集群侧特殊照顾。

pathshim 是独立通用项目，不认识 bed、sandbox 等上层业务概念。它不是安全边界，也不提供 rootfs、COW、host lower fallback、whiteout 或 mount namespace；需要完整根视图或安全隔离时应使用 PRoot、FUSE、bubblewrap 等对应机制。

## 关键约定

- **replace bind**：destination 内受支持的路径操作只落到 source，不读取或修改 Pod 中原有的 destination；映射外路径保持 Pod 原语义。
- **共享 source**：Projection 不持久化或缓存文件状态。多个独立 invocation 及外部 writer 可以并发共享 source，可见性、原子性和同文件写冲突遵循底层文件系统语义。
- **可启动性优先**：合法请求在 command 执行前发现 bind view 不可用时降级为 passthrough，以原 command、参数、继承环境和工作目录执行。
- **进程语义透明**：pathshim 不创建进程组，不承担 init/tini 职责；它继承 caller PGID、转发常见终止信号，并保留 command 的 exit/signal 终态。
- **覆盖必须诚实**：best effort 表示 syscall 覆盖不完整。未覆盖操作可能绕过映射访问 Pod 原路径，不得描述成完整 bind mount。

产品内核、运行流程、Projection 语义和能力边界统一见 [Kernel](docs/kernel.md)。

## 代码地图

```text
VERSION                 # pathshim 对外版本号
src/
├── main.rs             # invocation 主流程、bind-view/passthrough 选择和最终 exec
├── cli.rs              # repeatable bind、guest cwd、quiet 和 command 参数
├── bind.rs             # 无状态 BindView、Projection 与正反向路径选择
└── linux/
    ├── mod.rs          # command、seccomp listener、supervisor 与终态
    ├── dispatch.rs     # 文件系统 syscall 的 guest 语义
    ├── remote.rs       # tracee 内存、fd 与 cwd 状态访问
    ├── seccomp.rs      # seccomp user-notification 接口
    └── sysno.rs        # 架构相关 syscall 编号
tests/
├── linux_bind.rs       # replace、跨 invocation 并发、外部 writer、语言运行时与信号 E2E
├── linux_cwd.rs        # guest cwd 与进程语义 E2E
├── docker_smoke.sh     # 非特权容器 bind-view/passthrough smoke
├── ffmpeg_smoke.sh     # 调用方提供 ffmpeg 镜像的真实命令 smoke
└── capability_audit.sh # 报告未完整映射操作的现场 best-effort 行为
```

## 开发约定

- 每个可发布改动都必须递增根目录 `VERSION`，默认提升 patch 版本，并同步 `Cargo.toml` 与 `Cargo.lock`；CLI 测试负责校验版本源一致。
- 不把网络、权限限制和进程治理带入 pathshim；bind path mapping 是唯一职责。
- CLI 只表达 source、guest destination、cwd 和 command，不把 seccomp 等底层机制暴露成调用方概念。
- 普通运行输出实际模式和降级原因；集成方可用 `--quiet` 防止诊断混入 command stderr。
- Linux 行为测试至少覆盖动态链接程序、静态 Go 程序、子进程继承、多个独立 invocation 共享 source、外部 writer、guest cwd、映射外透传和 signal 终态。
- 正确性 E2E、依赖外部 command/image 的 smoke、以及只报告现场覆盖程度的 capability audit 保持不同 verdict，不能把 skip 或已知 bypass 描述成通过完整映射。

## References

- [Kernel](docs/kernel.md)：核心概念、执行流程、关键设计与已知边界。
- [README](README.md)：面向使用者的能力说明和最短使用路径。
