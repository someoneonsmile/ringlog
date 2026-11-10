//! ringlog：固定大小的单文件环形日志。
//!
//! - 写入：`producer | ringlog -s 64M app.ring`，写满后覆盖最旧数据，文件大小不变；
//! - 读取：`ringlog -f app.ring`，像 `tail -f` 一样跟随；
//! - 文件格式见 [`format`]。

#![deny(unsafe_code)]

pub mod cli;
pub mod follow;
pub mod format;
pub mod input;
pub mod ring;
pub mod seek;

/// 向 stderr 打一条告警。stderr 被关闭（告警通道不可用）时静默忽略，
/// 避免日志工具因为无法输出告警而崩溃。
pub(crate) fn warn(message: impl std::fmt::Display) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "ringlog: {message}");
}
