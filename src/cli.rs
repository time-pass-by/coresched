use crate::state::JobPriority;
/// CLI argument parsing — clap derive macros.
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "coresched",
    about = "CPU core scheduler for Legend",
    version,
    disable_help_subcommand = true,
    after_help = "优先级: --priority 0（默认）、1、2；队列按 2→1→0 派发。\n保留核: 默认 Core 6、7（逻辑 CPU 6,14,7,15）仅允许优先级 2 使用；P2 逻辑 CPU 优先顺序为 6,7,14,15。\n调整: coresched reserve-core 6 7；清除: coresched reserve-core --clear。"
)]
pub enum Cli {
    /// 提交作业并绑定 CPU 核心
    #[command(visible_alias = "r")]
    Run {
        /// CFD 作业: 分配 N 个物理核 (整个 core)
        #[arg(long, conflicts_with = "cpus", value_name = "N")]
        cfd: Option<u8>,

        /// Python 作业: 分配 N 个逻辑 CPU
        #[arg(long, conflicts_with = "cfd", value_name = "N")]
        cpus: Option<u8>,

        /// 优先级: 0(默认)、1、2
        #[arg(long, default_value_t = JobPriority::P0, value_name = "LEVEL")]
        priority: JobPriority,

        /// 分配超时 (秒), 0=无限等待 (默认: 0)
        #[arg(long, default_value = "0", value_name = "SECONDS")]
        timeout: u64,

        /// 后续命令 (after --)
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// 列出排队中的作业
    #[command(visible_alias = "ls")]
    List,

    /// 查看核心分配图
    #[command(visible_alias = "st")]
    Status,

    /// 取消作业 (可指定多个 JID，或 --all 取消全部)
    Cancel {
        /// 作业 ID (可多个，如 j0001 j0002)
        #[arg(value_name = "JID", required_unless_present = "all")]
        jids: Vec<String>,

        /// 取消所有作业（运行中 + 排队中）
        #[arg(long, conflicts_with = "jids")]
        all: bool,
    },

    /// 等待作业完成
    Wait {
        /// 作业 ID (如 j0001)
        jid: String,
    },

    /// 查看作业输出
    Tail {
        /// 作业 ID (如 j0001)
        jid: String,
    },

    /// 将作业加入内置队列（不占用资源，不 fork waiter 进程）
    #[command(visible_alias = "enq")]
    Enqueue {
        /// CFD 作业: 分配 N 个物理核 (整个 core)
        #[arg(long, conflicts_with = "cpus", value_name = "N")]
        cfd: Option<u8>,

        /// Python 作业: 分配 N 个逻辑 CPU
        #[arg(long, conflicts_with = "cfd", value_name = "N")]
        cpus: Option<u8>,

        /// 优先级: 0(默认)、1、2
        #[arg(long, default_value_t = JobPriority::P0, value_name = "LEVEL")]
        priority: JobPriority,

        /// 后续命令 (after --)
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// 保留一个或多个物理核，仅允许优先级 2 的任务使用
    #[command(visible_alias = "reserve")]
    ReserveCore {
        /// 物理核编号 (0-7)，可一次指定多个
        #[arg(
            value_name = "CORE",
            num_args = 1..,
            required_unless_present = "clear",
            conflicts_with = "clear"
        )]
        cores: Vec<u8>,

        /// 清除现有保留
        #[arg(long, conflicts_with = "cores")]
        clear: bool,
    },

    /// 查看作业仿真进度与预计剩余时间
    #[command(visible_alias = "pg")]
    Progress {
        /// 作业 ID (不指定则显示所有运行中 CFD 作业)
        jid: Option<String>,
    },
}

impl Cli {
    /// Parse CLI args and return the selected subcommand.
    pub fn parse_args() -> Self {
        <Self as Parser>::parse()
    }

    /// Return the subcommand name for display/logging.
    #[allow(dead_code)]
    pub fn command_name(&self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::List => "list",
            Self::Status => "status",
            Self::Cancel { .. } => "cancel",
            Self::Wait { .. } => "wait",
            Self::Tail { .. } => "tail",
            Self::Enqueue { .. } => "enqueue",
            Self::ReserveCore { .. } => "reserve-core",
            Self::Progress { .. } => "progress",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_parse_run_cfd() {
        let args = Cli::try_parse_from(&[
            "coresched",
            "run",
            "--cfd",
            "2",
            "--timeout",
            "0",
            "--",
            "echo",
            "hi",
        ]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Run {
                cfd, cpus, command, ..
            } => {
                assert_eq!(cfd, Some(2));
                assert_eq!(cpus, None);
                assert_eq!(command, vec!["echo".to_string(), "hi".to_string()]);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_run_cpus() {
        let args = Cli::try_parse_from(&[
            "coresched",
            "run",
            "--cpus",
            "4",
            "--timeout",
            "300",
            "--",
            "python3",
            "train.py",
        ]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Run {
                cfd,
                cpus,
                priority,
                ..
            } => {
                assert_eq!(cfd, None);
                assert_eq!(cpus, Some(4));
                assert_eq!(priority, JobPriority::P0);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_priority_two_enqueue() {
        let args = Cli::try_parse_from(&[
            "coresched",
            "enqueue",
            "--cpus",
            "1",
            "--priority",
            "2",
            "--",
            "echo",
            "hi",
        ]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Enqueue { priority, .. } => assert_eq!(priority, JobPriority::P2),
            _ => panic!("expected Enqueue"),
        }
    }

    #[test]
    fn test_parse_reserve_core() {
        let args = Cli::try_parse_from(&["coresched", "reserve-core", "7"]);
        assert!(matches!(
            args.unwrap(),
            Cli::ReserveCore { cores, clear: false } if cores == vec![7]
        ));
    }

    #[test]
    fn test_parse_multiple_reserved_cores() {
        let args = Cli::try_parse_from(&["coresched", "reserve-core", "6", "7"]);
        assert!(matches!(
            args.unwrap(),
            Cli::ReserveCore { cores, clear: false } if cores == vec![6, 7]
        ));
    }

    #[test]
    fn test_top_level_help_describes_priority_and_reservation() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--priority 0（默认）、1、2"));
        assert!(help.contains("reserve-core"));
    }

    #[test]
    fn test_parse_list() {
        let args = Cli::try_parse_from(&["coresched", "list"]);
        assert!(args.is_ok());
        assert!(matches!(args.unwrap(), Cli::List));
    }

    #[test]
    fn test_parse_status() {
        let args = Cli::try_parse_from(&["coresched", "status"]);
        assert!(args.is_ok());
        assert!(matches!(args.unwrap(), Cli::Status));
    }

    #[test]
    fn test_parse_cancel() {
        let args = Cli::try_parse_from(&["coresched", "cancel", "j0001"]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Cancel { jids, all } => {
                assert_eq!(jids, vec!["j0001"]);
                assert!(!all);
            }
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn test_parse_cancel_multiple() {
        let args =
            Cli::try_parse_from(&["coresched", "cancel", "j0001", "j0003", "j0007"]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Cancel { jids, all } => {
                assert_eq!(jids, vec!["j0001", "j0003", "j0007"]);
                assert!(!all);
            }
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn test_parse_cancel_all() {
        let args = Cli::try_parse_from(&["coresched", "cancel", "--all"]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Cancel { jids, all } => {
                assert!(jids.is_empty());
                assert!(all);
            }
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn test_parse_wait() {
        let args = Cli::try_parse_from(&["coresched", "wait", "j0001"]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Wait { jid } => assert_eq!(jid, "j0001"),
            _ => panic!("expected Wait"),
        }
    }

    #[test]
    fn test_parse_tail() {
        let args = Cli::try_parse_from(&["coresched", "tail", "j0001"]);
        assert!(args.is_ok());
        match args.unwrap() {
            Cli::Tail { jid } => assert_eq!(jid, "j0001"),
            _ => panic!("expected Tail"),
        }
    }
}
