# coresched

A small, zero-dependency **CPU core scheduler for Linux** — single Rust binary that
hands out physical cores to CFD/HPC jobs and logical CPUs to threaded/Python jobs,
with CPU-pinning via `sched_setaffinity`.

一个零依赖的 **Linux CPU 核心调度器** —— 单个 Rust 二进制文件，将物理核心分配给 CFD/HPC 作业，将逻辑 CPU 分配给多线程/Python 作业，通过 `sched_setaffinity` 进行 CPU 绑定。

## Features / 特性

- **Pure Rust / 纯 Rust** — 无需 `taskset`、`task-spooler` 或 Python 运行时。
- **Priority-aware queue / 优先级队列** (P0/P1/P2) — 标定任务优先于扫描任务；高优先级任务可独占指定物理核心。
- **Fork/exec model / 异步提交** — `run` 立即返回；子进程等待资源、分配核心、绑定并异步运行。
- **Durable enqueue / 持久化入队** — `enqueue` 写入持久化队列，进程重启不丢失；调度器在核心释放时自动派发。
- **OpenFOAM progress tracker / 仿真进度追踪** — 从进程树自动发现 case 目录和 `controlDict`，解析日志中的 `Time=`，计算 ETA。
- **Script progress / 脚本进度** — 脚本可写入 `{completed, total, eta_seconds, phase}` JSON 自定义进度上报。

## Install / 安装

```bash
cargo build --release
cp target/release/coresched ~/.local/bin/
```

Requires `cargo` + Rust toolchain (see [rustup.rs](https://rustup.rs)).
需要 `cargo` + Rust 工具链（参见 [rustup.rs](https://rustup.rs)）。

## Usage / 用法

```bash
# CFD job: 2 whole physical cores / 2个物理核心
coresched run --cfd 2 -- openfoam mySolver

# Python job: 4 logical CPUs, priority 1 / 4个逻辑CPU，优先级1
coresched run --cpus 4 --priority 1 -- python3 train.py

# Enqueue a batch / 批量入队（持久化，进程重启不丢）
coresched enqueue --cpus 4 -- python3 sweep.py 1
coresched enqueue --cpus 4 -- python3 sweep.py 2

# Inspect / 查看
coresched status         # core allocation map + ETA / 核心分配图+ETA
coresched list           # queued + running summary / 队列+运行摘要
coresched progress       # simulation progress / 仿真进度

# Manage / 管理
coresched cancel j0001   # cancel / 取消
coresched wait   j0001   # wait / 等待完成
coresched tail   j0001   # last 40 lines / 最后40行输出

# Reserve cores 6,7 for P2 only (default) / 保留核心仅P2可用
coresched reserve-core 6 7
coresched reserve-core --clear
```

## How it works / 工作原理

```
coresched (Rust binary)
  ├── cli.rs          clap argument parsing / 参数解析
  ├── main.rs         cmd_run / cmd_enqueue / cmd_list / cmd_status / …
  ├── scheduler.rs    priority-aware allocation / 优先级感知分配 (P2 → P1 → P0)
  ├── state.rs        ~/.coresched/state.json persistence / 持久化 (flock-guarded)
  ├── topology.rs     CPU topology / CPU 拓扑 (8C/16T, configurable)
  ├── process.rs      fork/exec/setsid/kill helpers / 进程管理
  ├── pin.rs          sched_setaffinity CPU pinning / CPU 绑定
  └── progress.rs     OpenFOAM log parser + ETA / 日志解析+ETA
```

State is stored at `~/.coresched/state.json`. The JSON schema is
backward-compatible with the original Bash/Python version.

状态数据存储在 `~/.coresched/state.json`，JSON 结构与原 Bash/Python 版本向后兼容。

## Priority & Core Reservation / 优先级与核心保留

| Priority | Typical use / 典型场景 | Reserved core access / 保留核心 |
|----------|----------------------|------------------------------|
| P0 (default) | Sweep / batch jobs / 批量扫描 | Excluded / 不可用 |
| P1 | Above-normal batch / 较重要批量 | Excluded / 不可用 |
| P2 | Calibration / critical / 标定/关键 | Prefers reserved cores (default: 6,7) / 优先使用保留核心 |

Default reserved cores: 6,7 → logical CPUs / 逻辑CPU 6,14,7,15 only available to P2 jobs / 仅供 P2 使用。

## License / 许可证

[MIT](LICENSE)
