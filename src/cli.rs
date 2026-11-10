//! 命令行参数解析（clap derive）。
//!
//! 解析职责交给 clap：选项、`--help`/`--version`、`--opt=value`、错误提示与退出码
//! 都由 clap 生成；本模块只保留 ringlog 自己的规则：模式互斥、`-s` 的尺寸语义、
//! 以及 `-n 0` 必须跟 `-f` 一起用这类跨参数校验。

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

use crate::format::{HEADER_PAGE, MIN_DATA_CAPACITY};

const ABOUT: &str = "固定大小的单文件环形日志";

const HELP_TEMPLATE: &str = "\
ringlog —— 固定大小的单文件环形日志

用法:
  {usage}

参数:
{positionals}

选项:
{options}
{after-help}";

const AFTER_HELP: &str = "\
常用用法:
  写入          producer | ringlog -s 64M /var/log/app.ring
  跟随          ringlog -f -n 100 /var/log/app.ring
  只看新数据    ringlog -f -n 0 /var/log/app.ring
  看最近 N 行   ringlog -n 200 /var/log/app.ring
  看元信息      ringlog --info /var/log/app.ring

说明:
  - 同一文件同一时刻只允许一个写入进程（用 <FILE>.lock 上的 flock 互斥）。
  - 文件大小固定不变；磁盘块按需分配，占用上限即 -s。
  - 记录以换行结尾；空间不足时用空行填充到文件尾，保证记录不被环形边界拆断。
  - 单行超过数据区容量时截断并保留行首，写入进程退出时汇总告警。
  - 读取进程落后超过数据区容量时告警并跳到最旧数据继续。";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Write,
    Read,
    Info,
}

#[derive(Debug)]
pub struct Cli {
    pub path: PathBuf,
    pub mode: Mode,
    /// 仅写入模式：文件总大小。
    pub size: Option<u64>,
    /// 仅读取模式：最近行数；`Some(0)` 表示只看新数据。
    pub lines: Option<u64>,
    pub follow: bool,
    pub sync: bool,
    pub force: bool,
    pub interval: Duration,
}

#[derive(Debug, Parser)]
#[command(
    name = "ringlog",
    version,
    about = ABOUT,
    help_template = HELP_TEMPLATE,
    after_help = AFTER_HELP,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    /// 文件总大小，如 64M、1.5G、10485760（含 4 KiB 头区，至少 8K）
    #[arg(short = 's', long = "size", value_name = "SIZE", value_parser = parse_size)]
    size: Option<u64>,

    /// 跟随模式：先输出历史数据，再持续输出新数据
    #[arg(short = 'f', long = "follow")]
    follow: bool,

    /// 最近 N 行；与 -f 合用时 N=0 表示跳过历史、只看新数据
    #[arg(short = 'n', long = "lines", value_name = "N")]
    lines: Option<u64>,

    /// 跟随轮询间隔（毫秒，默认 100）
    #[arg(long = "interval-ms", value_name = "MS", default_value_t = 100)]
    interval_ms: u64,

    /// 每一批记录后 fsync（更抗断电，吞吐更低）
    #[arg(long = "sync")]
    sync: bool,

    /// 目标文件大小/头部不匹配时，丢弃旧数据重建
    #[arg(long = "force")]
    force: bool,

    /// 打印容量、写位置、累计写入等元信息
    #[arg(long = "info")]
    info: bool,

    /// 环形日志文件路径
    #[arg(value_name = "FILE")]
    file: PathBuf,

    /// 显示帮助
    #[arg(short = 'h', long = "help", action = clap::ArgAction::Help)]
    help: Option<bool>,

    /// 显示版本
    #[arg(short = 'V', long = "version", action = clap::ArgAction::Version)]
    version: Option<bool>,
}

/// 解析命令行。`--help` / `--version` 以及参数错误都以 [`clap::Error`] 返回，
/// 由调用方决定打印目标和退出码（clap 的约定：帮助/版本退出码 0，错误 2）。
pub fn parse<I>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = OsString>,
{
    let argv = std::iter::once(OsString::from("ringlog")).chain(args);
    Args::try_parse_from(argv)?.into_cli()
}

impl Args {
    fn into_cli(self) -> Result<Cli, clap::Error> {
        let Args {
            size,
            follow,
            lines,
            interval_ms,
            sync,
            force,
            info,
            file,
            help: _,
            version: _,
        } = self;

        let mode = if info {
            if size.is_some() || lines.is_some() || follow || sync || force {
                return Err(usage_error(
                    "--info 不能与 -s / -n / -f / --sync / --force 同时使用",
                ));
            }
            Mode::Info
        } else if follow || lines.is_some() {
            if size.is_some() {
                return Err(usage_error("读取模式（-f / -n）不能与 -s 同时使用"));
            }
            if sync || force {
                return Err(usage_error("--sync / --force 只用于写入模式"));
            }
            if !follow && lines == Some(0) {
                return Err(usage_error("-n 0 只能与 -f 一起使用（表示只看新数据）"));
            }
            Mode::Read
        } else {
            let size = size.ok_or_else(|| {
                usage_error(
                    "写入模式需要 -s SIZE（例如 -s 64M）；读取用 -f / -n，查看信息用 --info",
                )
            })?;
            let min = HEADER_PAGE + MIN_DATA_CAPACITY;
            if size < min {
                return Err(usage_error(format!(
                    "-s 至少为 {min} 字节（4 KiB 头区 + 4 KiB 数据区）"
                )));
            }
            Mode::Write
        };

        Ok(Cli {
            path: file,
            mode,
            size,
            lines,
            follow,
            sync,
            force,
            interval: Duration::from_millis(interval_ms),
        })
    }
}

/// 把跨参数校验失败包装成 clap 的用法错误（退出码 2，并带使用方法提示）。
fn usage_error(message: impl Into<String>) -> clap::Error {
    clap::Error::raw(ErrorKind::ValueValidation, message.into()).with_cmd(&Args::command())
}

/// 解析大小：`10485760`、`64K`、`64KiB`、`1G`、`2TB`、`1.5G`（K/M/G/T 均为 1024 进制）。
pub fn parse_size(text: &str) -> Result<u64, String> {
    match parse_size::Config::new().with_binary().parse_size(text) {
        Ok(0) => Err("大小不能为 0".into()),
        Ok(bytes) => Ok(bytes),
        Err(parse_size::Error::PosOverflow) => Err(format!("大小 {text:?} 超出范围")),
        Err(_) => Err(format!("无法解析大小 {text:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Cli, clap::Error> {
        parse(args.iter().map(|s| OsString::from(*s)))
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("64").unwrap(), 64);
        assert_eq!(parse_size("64k").unwrap(), 64 * 1024);
        assert_eq!(parse_size("64KiB").unwrap(), 64 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_size("2TB").unwrap(), 2 << 40);
        assert_eq!(parse_size("64KB").unwrap(), 64 * 1024);
        assert_eq!(parse_size("1.5G").unwrap(), 3 << 29);
        assert!(parse_size("0").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-1G").is_err());
    }

    #[test]
    fn write_mode_requires_size() {
        assert!(parse_args(&["/tmp/x.ring"]).is_err());
        let cli = parse_args(&["-s", "64K", "/tmp/x.ring"]).unwrap();
        assert_eq!(cli.mode, Mode::Write);
        assert_eq!(cli.size, Some(64 * 1024));
    }

    #[test]
    fn size_too_small_rejected() {
        assert!(parse_args(&["-s", "4K", "/tmp/x.ring"]).is_err());
    }

    #[test]
    fn follow_mode_without_size() {
        let cli = parse_args(&["-f", "/tmp/x.ring"]).unwrap();
        assert_eq!(cli.mode, Mode::Read);
        assert!(cli.follow);
        assert_eq!(cli.lines, None);
    }

    #[test]
    fn tail_mode() {
        let cli = parse_args(&["-n", "10", "/tmp/x.ring"]).unwrap();
        assert_eq!(cli.mode, Mode::Read);
        assert!(!cli.follow);
        assert_eq!(cli.lines, Some(10));
    }

    #[test]
    fn n_zero_requires_follow() {
        assert!(parse_args(&["-n", "0", "/tmp/x.ring"]).is_err());
        assert!(parse_args(&["-f", "-n", "0", "/tmp/x.ring"]).is_ok());
    }

    #[test]
    fn rejects_unknown_option() {
        assert!(parse_args(&["--nope", "/tmp/x.ring"]).is_err());
    }

    #[test]
    fn rejects_mixing_write_and_read_options() {
        assert!(parse_args(&["-s", "64K", "-f", "/tmp/x.ring"]).is_err());
    }

    #[test]
    fn info_mode() {
        let cli = parse_args(&["--info", "/tmp/x.ring"]).unwrap();
        assert_eq!(cli.mode, Mode::Info);
    }

    #[test]
    fn help_and_version_are_not_errors() {
        let help = parse_args(&["--help"]).unwrap_err();
        assert_eq!(help.exit_code(), 0);
        assert!(help.to_string().contains("用法:"));

        let version = parse_args(&["--version"]).unwrap_err();
        assert_eq!(version.exit_code(), 0);
        assert!(version.to_string().contains("ringlog"));
    }

    #[test]
    fn errors_exit_two() {
        for args in [&["--nope"][..], &[][..], &["-n", "0", "/tmp/x.ring"][..]] {
            let err = parse_args(args).unwrap_err();
            assert_eq!(err.exit_code(), 2, "args={args:?}");
        }
    }
}
