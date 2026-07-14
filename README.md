# coresched

一个零依赖的 **Linux CPU 核心调度器** —— 单个 Rust 二进制文件，将物理核心分配给 CFD/HPC 作业，将逻辑 CPU 分配给多线程/Python 作业，通过 `sched_setaffinity` 进行 CPU 绑定。

A small, zero-dependency **CPU core scheduler for Linux** — single Rust binary that
hands out physical cores to CFD/HPC jobs and logical CPUs to threaded/Python jobs,
with CPU-pinning via `sched_setaffinity`.

## 特性 / Features

- **纯 Rust / Pure Rust** — 无需 `taskset`、`task-spooler` 或 Python 运行时。
- **优先级队列 / Priority-aware queue** (P0/P1/P2) — 标定任务优先于扫描任务；高优先级任务可独占指定物理核心。
- **异步提交 / Fork/exec model** — `run` 立即返回；子进程等待资源、分配核心、绑定并异步运行。
- **持久化入队 / Durable enqueue** — `enqueue` 写入持久化队列，进程重启不丢失；调度器在核心释放时自动派发。
- **仿真进度追踪 / OpenFOAM progress tracker** — 从进程树自动发现 case 目录和 `controlDict`，解析日志中的 `Time=`，计算 ETA。
- **脚本进度 / Script progress** — 脚本可写入 `{completed, total, eta_seconds, phase}` JSON 自定义进度上报。

## 安装 / Install

```bash
cargo build --release
cp target/release/coresched ~/.local/bin/
```

需要 `cargo` + Rust 工具链（参见 [rustup.rs](https://rustup.rs)）。
Requires `cargo` + Rust toolchain (see [rustup.rs](https://rustup.rs)).

## 用法 / Usage

```bash
# CFD 作业: 2个物理核心 / CFD job: 2 whole physical cores
coresched run --cfd 2 -- openfoam mySolver

# Python 作业: 4个逻辑CPU，优先级1 / 4 logical CPUs, priority 1
coresched run --cpus 4 --priority 1 -- python3 train.py

# 批量入队（持久化，进程重启不丢）/ Enqueue a batch
coresched enqueue --cpus 4 -- python3 sweep.py 1
coresched enqueue --cpus 4 -- python3 sweep.py 2

# 查看 / Inspect
coresched status         # 核心分配图+ETA / core allocation map + ETA
coresched list           # 队列+运行摘要 / queued + running summary
coresched progress       # 仿真进度 / simulation progress

# 管理 / Manage
coresched cancel j0001   # 取消 / cancel
coresched wait   j0001   # 等待完成 / wait
coresched tail   j0001   # 最后40行输出 / last 40 lines

# 保留核心仅P2可用 / Reserve cores for P2 only
coresched reserve-core 6 7
coresched reserve-core --clear
```

## 工作原理 / How it works

```
coresched (Rust binary)
  ├── cli.rs          参数解析 / arg parsing
  ├── main.rs         cmd_run / cmd_enqueue / cmd_list / cmd_status / …
  ├── scheduler.rs    优先级感知分配 / priority-aware allocation (P2 → P1 → P0)
  ├── state.rs        ~/.coresched/state.json 持久化 / persistence (flock-guarded)
  ├── topology.rs     CPU 拓扑自动探测 / auto-detected topology
  ├── process.rs      进程管理 / fork/exec/setsid/kill
  ├── pin.rs          CPU 绑定 / sched_setaffinity pinning
  └── progress.rs     日志解析+ETA / log parser + ETA
```

状态数据存储在 `~/.coresched/state.json`，JSON 结构与原 Bash/Python 版本向后兼容。

State is stored at `~/.coresched/state.json`. The JSON schema is
backward-compatible with the original Bash/Python version.

## 优先级与核心保留 / Priority & Core Reservation

| 优先级 / Priority | 典型场景 / Typical use | 保留核心 / Reserved core access |
|----------|----------------------|------------------------------|
| P0（默认 / default） | 批量扫描 / Sweep | 不可用 / Excluded |
| P1 | 较重要批量 / Above-normal batch | 不可用 / Excluded |
| P2 | 标定/关键 / Calibration | 优先使用保留核心 / Prefers reserved cores |

默认保留末尾若干物理核（总核数 ≥ 4 时）。

Default reserved cores: last ~ceil(N/4) cores when N >= 4.

## 许可证 / License

[MIT](LICENSE)
