//! 终端输出的唯一出口。
//!
//! stdout / stderr 写失败都静默忽略（下游关闭管道、stderr 不可用时不应 panic）。

use std::fmt::Display;
use std::io::{self, BufWriter, StdoutLock, Write};

/// 缓冲 stdout 的容量。
const STDOUT_BUFFER: usize = 64 * 1024;

/// 向 stdout 写一段文本。
pub fn stdout(text: &str) {
    let _ = io::stdout().write_all(text.as_bytes());
}

/// 流式输出用的带缓冲 stdout。
pub fn buffered_stdout() -> BufWriter<StdoutLock<'static>> {
    BufWriter::with_capacity(STDOUT_BUFFER, io::stdout().lock())
}

/// 向 stderr 打一条带 `ringlog: ` 前缀的告警。
pub fn warn(message: impl Display) {
    let _ = writeln!(io::stderr(), "ringlog: {message}");
}
