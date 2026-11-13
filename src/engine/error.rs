//! 错误分类。
//!
//! 库里的错误统一用 [`io::Error`]，但按语义分成三类：
//! - 用法错误（参数/环境不满足）：[`io::ErrorKind::InvalidInput`]；
//! - 数据错误（文件损坏、格式不匹配）：[`io::ErrorKind::InvalidData`]；
//! - 普通 I/O 错误：其余。
//!
//! 前两类是「用户能看懂并纠正的问题」，命令行据此返回退出码 2；其余返回 1。

use std::io;

/// 构造「用法错误」。
pub(super) fn usage(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// 构造「数据错误」。
pub(super) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// 该错误是否属于「用法/数据错误」。
pub fn is_user_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData
    )
}
