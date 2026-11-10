//! ringlog 引擎：固定大小的单文件环形日志。
//!
//! 本库只负责文件格式与读写，不接触终端：
//! - [`format`]：文件布局、头部编解码与尺寸约束；
//! - [`ring`]：写者 [`ring::Ring`]、读者 [`ring::Reader`] 与锁文件；
//! - [`input`]：把字节流切成以换行结尾的记录；
//! - [`seek`]：在环形窗口里定位首屏起点；
//! - [`error`]：错误分类（用法/数据错误 vs 普通 I/O 错误）。
//!
//! 命令行应用（参数解析、`-f` 跟随、stdout/stderr 输出）在 `src/app/`，不属于本库。

#![deny(unsafe_code)]

pub mod error;
pub mod format;
pub mod input;
pub mod ring;
pub mod seek;
