// clap 命令行定义。
//
// ⚠️ 本文件被 build.rs 通过 include! 引入（用于生成 shell 补全），
// 禁止在这里使用 crate:: 路径或应用层类型；clap 之外的依赖只允许 parse-size。

use std::path::PathBuf;

use clap::Parser;

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
pub struct Args {
    /// 文件总大小，如 64M、1.5G、10485760（含 4 KiB 头区，至少 8K，最多 1T）
    #[arg(short = 's', long = "size", value_name = "SIZE", value_parser = parse_size)]
    pub size: Option<u64>,

    /// 跟随模式：先输出历史数据，再持续输出新数据
    #[arg(short = 'f', long = "follow")]
    pub follow: bool,

    /// 最近 N 行；与 -f 合用时 N=0 表示跳过历史、只看新数据
    #[arg(short = 'n', long = "lines", value_name = "N")]
    pub lines: Option<u64>,

    /// 跟随轮询间隔（毫秒，默认 20）
    #[arg(long = "interval-ms", value_name = "MS", default_value_t = 20)]
    pub interval_ms: u64,

    /// 每一批记录后 fsync（更抗断电，吞吐更低）
    #[arg(long = "sync")]
    pub sync: bool,

    /// 目标文件大小/头部不匹配时，丢弃旧数据重建
    #[arg(long = "force")]
    pub force: bool,

    /// 打印容量、写位置、累计写入等元信息
    #[arg(long = "info")]
    pub info: bool,

    /// 环形日志文件路径
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// 显示帮助
    #[arg(short = 'h', long = "help", action = clap::ArgAction::Help)]
    pub help: Option<bool>,

    /// 显示版本
    #[arg(short = 'V', long = "version", action = clap::ArgAction::Version)]
    pub version: Option<bool>,
}

/// `-s` 允许的最大文件总大小（1 TiB）。
///
/// 上限属于命令行策略而非文件格式：格式本身能表达任意 `u64` 容量，但环形日志
/// 用不到 1 TiB 以上；而常见文件系统的单文件上限各不相同（如 ext4 为 16 TiB），
/// 统一挡住可以避免 `-s` 过大时只在部分文件系统上报 EFBIG。
pub const MAX_TOTAL_SIZE: u64 = 1 << 40;

/// 解析大小：`10485760`、`64K`、`64KiB`、`1G`、`1T`、`1.5G`（K/M/G/T 均为 1024 进制）。
pub fn parse_size(text: &str) -> Result<u64, String> {
    match parse_size::Config::new().with_binary().parse_size(text) {
        Ok(0) => Err("大小不能为 0".into()),
        Ok(bytes) => Ok(bytes),
        Err(parse_size::Error::PosOverflow) => Err(format!("大小 {text:?} 超出范围")),
        Err(_) => Err(format!("无法解析大小 {text:?}")),
    }
}
