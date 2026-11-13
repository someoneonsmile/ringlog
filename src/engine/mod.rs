//! 引擎门面：固定大小的单文件环形日志。
//!
//! 子模块全部私有，命令行层只通过本文件导出的项访问引擎；引擎内部的类型、
//! 常量与辅助函数不会越过这条边界。
//!
//! 门面清单：
//! - [`Reader`] / [`Ring`]：读者与写者；
//! - [`LineSplitter`]：把字节流切成以换行结尾的记录；
//! - [`StartPos`] / [`locate_start`] / [`scan_to_next_line`]：首屏与窗口定位；
//! - [`MIN_TOTAL_SIZE`]、[`is_user_error`]：尺寸约束与错误分类。
//!
//! `Header` 与 `Located` 分别是 `locate_start` / `scan_to_next_line` 的参数与返回类型，
//! 调用方按闭包参数或字段访问使用，不必命名类型，因此不单独导出；`lock_path`
//! 是写者互斥的实现细节，由 `Ring::open` 内部调用，也不对外提供。

mod error;
mod format;
mod input;
mod ring;
mod seek;

pub(crate) use error::is_user_error;
pub(crate) use format::MIN_TOTAL_SIZE;
pub(crate) use input::LineSplitter;
pub(crate) use ring::{Reader, Ring};
pub(crate) use seek::{StartPos, locate_start, scan_to_next_line};
