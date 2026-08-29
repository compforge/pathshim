# AGENTS.md

## 项目定位与边界

pathshim 是运行在普通 Kubernetes Pod 内的 best-effort 文件系统视图工具。它不得要求 Pod 具备 `privileged`、额外 capability、自定义 seccomp/AppArmor、`/dev/fuse` 或其它集群侧特殊照顾；如果部署环境能够提供 mount namespace 等能力，应直接选择 bubblewrap，而不是让 pathshim 重复实现特权方案。

pathshim 是独立的通用项目，通过根映射或局部 bind 把进程运行产物尽量收拢到指定目录，不绑定任何上层业务概念。其产品目标与 PRoot 一致：为非特权进程提供 guest 文件系统视图；区别是 pathshim 接受能力降级与 best-effort 覆盖，不追求 PRoot 基于 ptrace 的完整 syscall 仿真。它不是安全边界，也不承诺等价于 `chroot`、mount namespace 或容器文件系统。

## 关键约定

- **可启动性优先**：文件收拢能力是可选增强。合法请求在 command 执行前发现能力不足时必须逐级降级，末级以调用者原始 command、参数、继承环境和工作目录执行。
- **进程语义透明**：pathshim 只是文件系统视图包装层，不应因为多经过一层进程就改变 caller 对进程组、信号和终态的判断。当前实现边界与未闭合部分见 [Kernel](docs/kernel.md#进程托管与终态)。
- **文件系统视图一致**：同一次 invocation 中，command 及其所有后代必须共享确定的映射；best effort 只限制 syscall 覆盖范围，不允许已支持的路径因为并发或调用方式改变读写落点。
- pathshim 只认识 source、guest destination 和 command，不认识 bed、sandbox 等 caller 业务概念，也不主动创建或改写 caller 的目录布局与环境约定。
- 一个 projection source 同时只由一个活跃 invocation 持有；独立 invocation 并发共享 source 不在契约内。

产品内核、运行流程、projection 语义和降级路径统一见 [Kernel](docs/kernel.md)。

## 代码地图

```text
src/
├── main.rs             # invocation 主流程、能力降级与最终 exec
├── cli.rs              # rootfs、bind、guest cwd 和 command 参数
├── root.rs             # Projection/RootView、COW、whiteout 与路径选择
└── linux/
    ├── mod.rs          # command、seccomp listener 与 supervisor 生命周期
    ├── dispatch.rs     # 文件系统 syscall 的 guest 语义
    ├── remote.rs       # tracee 内存、fd 与路径状态访问
    ├── seccomp.rs      # seccomp user-notification 接口
    └── sysno.rs        # 架构相关 syscall 编号
tests/
├── linux_cow.rs        # rootfs/bind、并发视图、Python/静态 Go 与信号 E2E
├── linux_cwd.rs        # guest cwd 与进程语义 E2E
├── docker_smoke.sh     # 非特权容器 cow-view/cwd/passthrough smoke test
├── ffmpeg_smoke.sh     # 调用方提供 ffmpeg 镜像的真实转码 smoke test
└── capability_audit.sh # 报告未完整映射操作的现场 best-effort 行为
```

## 开发约定

- 不把网络、进程、权限限制等 sandbox 策略带入 pathshim；文件收拢是唯一职责。
- CLI 与调用契约只表达 source 对应 guest destination 的语义，不把 seccomp、ptrace、FUSE 等机制变成参数或上层概念；README 可以用机制对比解释能力边界和现场要求。
- 运行日志应说明实际启用的收拢能力和降级原因，避免调用方误判覆盖范围。
- 文件系统行为测试至少覆盖动态链接程序和静态 Go 程序，并验证根映射与 bind 的读取回退、写入 upper、映射外透传、子进程继承、guest cwd 和重复执行后的持久性；cwd 场景按 PRoot 的测试思路拆成独立 fixture 与集成测试。
- 正确性 E2E、依赖外部 command/image 的 smoke、以及只报告现场覆盖程度的 capability audit 必须保持不同 verdict，不能把 skip 或已知 lower bypass 描述成通过完整映射。

## References

- [Kernel](docs/kernel.md)：核心概念、执行流程、关键设计与已知边界。
- [README](README.md)：面向使用者的能力说明和最短使用路径。
