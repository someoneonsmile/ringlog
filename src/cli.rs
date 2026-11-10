//! 命令行参数解析（不依赖第三方库）。

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::format::{HEADER_PAGE, MIN_DATA_CAPACITY};

pub const HELP: &str = "\
ringlog —— 固定大小的单文件环形日志

用法:
  ringlog -s SIZE <FILE>            写入：stdin 管道 → 环形文件（写满覆盖最旧数据）
  ringlog -f [-n N] <FILE>          读取：输出已有数据后持续跟随（类似 tail -f）
  ringlog -n N <FILE>               读取：输出最近 N 行后退出
  ringlog --info <FILE>             查看文件元信息

选项:
  -s, --size SIZE       文件总大小，如 64M、1G、10485760（含 4 KiB 头区，至少 8K）
  -f, --follow          跟随模式：先输出历史数据，再持续输出新数据
  -n, --lines N         最近 N 行；与 -f 合用时 N=0 表示跳过历史、只看新数据
      --interval-ms MS  跟随轮询间隔（默认 100）
      --sync            每一批记录后 fsync（更抗断电，吞吐更低）
      --force           目标文件大小/头部不匹配时，丢弃旧数据重建
      --info            打印容量、写位置、累计写入等元信息
  -h, --help            显示帮助
  -V, --version         显示版本

说明:
  - 同一文件同一时刻只允许一个写入进程（用 <FILE>.lock 上的 flock 互斥）。
  - 文件大小固定不变；磁盘块按需分配，占用上限即 -s。
  - 记录以换行结尾；空间不足时用空行填充到文件尾，保证记录不被环形边界拆断。
  - 单行超过数据区容量时截断并保留行首，写入进程退出时汇总告警。
  - 读取进程落后超过数据区容量时告警并跳到最旧数据继续。
";

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

pub fn parse<I>(args: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let mut path: Option<PathBuf> = None;
    let mut size: Option<u64> = None;
    let mut lines: Option<u64> = None;
    let mut follow = false;
    let mut sync = false;
    let mut force = false;
    let mut info = false;
    let mut interval = Duration::from_millis(100);

    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            set_path(&mut path, PathBuf::from(arg))?;
            continue;
        };
        if let Some(value) = text.strip_prefix("--size=") {
            size = Some(parse_size(value)?);
        } else if let Some(value) = text.strip_prefix("--lines=") {
            lines = Some(parse_count(value, "--lines")?);
        } else if let Some(value) = text.strip_prefix("--interval-ms=") {
            interval = Duration::from_millis(parse_count(value, "--interval-ms")?);
        } else {
            match text {
                "-s" | "--size" => size = Some(parse_size(&take_value(&mut args, text)?)?),
                "-n" | "--lines" => lines = Some(parse_count(&take_value(&mut args, text)?, text)?),
                "-f" | "--follow" => follow = true,
                "--sync" => sync = true,
                "--force" => force = true,
                "--info" => info = true,
                "--interval-ms" => {
                    interval =
                        Duration::from_millis(parse_count(&take_value(&mut args, text)?, text)?)
                }
                other if other.starts_with('-') && other != "-" => {
                    return Err(format!("未知选项 {other}"));
                }
                _ => set_path(&mut path, PathBuf::from(text))?,
            }
        }
    }

    let path = path.ok_or("缺少 <FILE> 参数")?;
    let mode = if info {
        if size.is_some() || lines.is_some() || follow || sync || force {
            return Err("--info 不能与 -s / -n / -f / --sync / --force 同时使用".into());
        }
        Mode::Info
    } else if follow || lines.is_some() {
        if size.is_some() {
            return Err("读取模式（-f / -n）不能与 -s 同时使用".into());
        }
        if sync || force {
            return Err("--sync / --force 只用于写入模式".into());
        }
        if !follow && lines == Some(0) {
            return Err("-n 0 只能与 -f 一起使用（表示只看新数据）".into());
        }
        Mode::Read
    } else {
        let size =
            size.ok_or("写入模式需要 -s SIZE（例如 -s 64M）；读取用 -f / -n，查看信息用 --info")?;
        let min = HEADER_PAGE + MIN_DATA_CAPACITY;
        if size < min {
            return Err(format!("-s 至少为 {min} 字节（4 KiB 头区 + 4 KiB 数据区）"));
        }
        Mode::Write
    };

    Ok(Cli {
        path,
        mode,
        size,
        lines,
        follow,
        sync,
        force,
        interval,
    })
}

fn set_path(slot: &mut Option<PathBuf>, path: PathBuf) -> Result<(), String> {
    if slot.is_some() {
        return Err("只能指定一个 <FILE> 参数".into());
    }
    *slot = Some(path);
    Ok(())
}

fn take_value<I: Iterator<Item = OsString>>(args: &mut I, flag: &str) -> Result<String, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("{flag} 需要一个参数值"))?;
    value
        .into_string()
        .map_err(|_| format!("{flag} 的参数不是合法的 UTF-8 文本"))
}

fn parse_count(text: &str, flag: &str) -> Result<u64, String> {
    text.trim()
        .parse::<u64>()
        .map_err(|_| format!("{flag} 需要一个非负整数，收到 {text:?}"))
}

/// 解析大小：`10485760`、`64K`、`64KiB`、`1G`、`2TB`（K/M/G/T 均为 1024 进制）。
pub fn parse_size(text: &str) -> Result<u64, String> {
    let original = text;
    let lower = text.trim().to_ascii_lowercase();
    let unit_stripped = lower
        .strip_suffix("ib")
        .or_else(|| lower.strip_suffix('b'))
        .unwrap_or(&lower);
    let (digits, multiplier) = if let Some(num) = unit_stripped.strip_suffix('k') {
        (num, 1u64 << 10)
    } else if let Some(num) = unit_stripped.strip_suffix('m') {
        (num, 1u64 << 20)
    } else if let Some(num) = unit_stripped.strip_suffix('g') {
        (num, 1u64 << 30)
    } else if let Some(num) = unit_stripped.strip_suffix('t') {
        (num, 1u64 << 40)
    } else {
        (unit_stripped, 1)
    };
    let value = digits
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("无法解析大小 {original:?}"))?;
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("大小 {original:?} 超出范围"))?;
    if bytes == 0 {
        return Err("大小不能为 0".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Cli, String> {
        parse(args.iter().map(|s| OsString::from(*s)))
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("64").unwrap(), 64);
        assert_eq!(parse_size("64k").unwrap(), 64 * 1024);
        assert_eq!(parse_size("64KiB").unwrap(), 64 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_size("2TB").unwrap(), 2 << 40);
        assert!(parse_size("0").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("1.5G").is_err());
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
}
