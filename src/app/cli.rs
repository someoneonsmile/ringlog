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

use ringlog::format::MIN_TOTAL_SIZE;
use ringlog::seek::StartPos;

use super::cli_types::{Args, MAX_TOTAL_SIZE};

/// 命令行模式。每种模式只携带自己用得到的参数，非法组合无法表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 写入：stdin 管道 → 环形文件。
    Write { size: u64, sync: bool, force: bool },
    /// 读取：先输出已有数据，可选持续跟随。
    Read {
        follow: bool,
        start: StartPos,
        interval: Duration,
    },
    /// 打印文件元信息。
    Info,
}

#[derive(Debug)]
pub struct Cli {
    pub path: PathBuf,
    pub mode: Mode,
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
            let start = match lines {
                None => StartPos::Oldest,
                Some(0) if follow => StartPos::Newest,
                Some(0) => {
                    return Err(usage_error("-n 0 只能与 -f 一起使用（表示只看新数据）"));
                }
                Some(n) => StartPos::Last(n),
            };
            Mode::Read {
                follow,
                start,
                interval: Duration::from_millis(interval_ms),
            }
        } else {
            let size = size.ok_or_else(|| {
                usage_error(
                    "写入模式需要 -s SIZE（例如 -s 64M）；读取用 -f / -n，查看信息用 --info",
                )
            })?;
            if size < MIN_TOTAL_SIZE {
                return Err(usage_error(format!(
                    "-s 至少为 {MIN_TOTAL_SIZE} 字节（4 KiB 头区 + 4 KiB 数据区）"
                )));
            }
            if size > MAX_TOTAL_SIZE {
                return Err(usage_error(format!(
                    "-s 最大为 {MAX_TOTAL_SIZE} 字节（1 TiB）"
                )));
            }
            Mode::Write { size, sync, force }
        };

        Ok(Cli { path: file, mode })
    }
}

/// 把跨参数校验失败包装成 clap 的用法错误（退出码 2，并带使用方法提示）。
fn usage_error(message: impl Into<String>) -> clap::Error {
    clap::Error::raw(ErrorKind::ValueValidation, message.into()).with_cmd(&Args::command())
}

#[cfg(test)]
mod tests {
    use super::*;
    // 只在测试里直接用：clap 通过 value_parser 调用它。
    use super::super::cli_types::parse_size;

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
        assert_eq!(
            cli.mode,
            Mode::Write {
                size: 64 * 1024,
                sync: false,
                force: false
            }
        );
    }

    #[test]
    fn size_too_small_rejected() {
        assert!(parse_args(&["-s", "4K", "/tmp/x.ring"]).is_err());
    }

    #[test]
    fn size_too_large_rejected() {
        // 1 TiB 正好允许，多 1 字节就拒绝。
        assert!(parse_args(&["-s", "1T", "/tmp/x.ring"]).is_ok());
        let over = (MAX_TOTAL_SIZE + 1).to_string();
        let err = parse_args(&["-s", &over, "/tmp/x.ring"]).unwrap_err();
        assert!(err.to_string().contains("1 TiB"), "err={err}");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn follow_mode_without_size() {
        let cli = parse_args(&["-f", "/tmp/x.ring"]).unwrap();
        assert_eq!(
            cli.mode,
            Mode::Read {
                follow: true,
                start: StartPos::Oldest,
                interval: Duration::from_millis(20),
            }
        );
    }

    #[test]
    fn tail_mode() {
        let cli = parse_args(&["-n", "10", "/tmp/x.ring"]).unwrap();
        assert_eq!(
            cli.mode,
            Mode::Read {
                follow: false,
                start: StartPos::Last(10),
                interval: Duration::from_millis(20),
            }
        );
    }

    #[test]
    fn follow_poll_interval_is_parsed() {
        let cli = parse_args(&["-f", "--interval-ms", "250", "/tmp/x.ring"]).unwrap();
        let Mode::Read { interval, .. } = cli.mode else {
            panic!("应为读取模式，实际 {:?}", cli.mode);
        };
        assert_eq!(interval, Duration::from_millis(250));
    }

    #[test]
    fn n_zero_requires_follow() {
        assert!(parse_args(&["-n", "0", "/tmp/x.ring"]).is_err());
        let cli = parse_args(&["-f", "-n", "0", "/tmp/x.ring"]).unwrap();
        assert_eq!(
            cli.mode,
            Mode::Read {
                follow: true,
                start: StartPos::Newest,
                interval: Duration::from_millis(20),
            }
        );
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
