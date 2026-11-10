//! ringlog 应用层：命令行解析、终端输出与三种运行模式。
//!
//! 引擎（文件格式、环形读写、定位）在 `ringlog` 库里；这里只做 I/O 与用户交互。

pub mod info;
pub mod report;
pub mod writer;
