//! `--info`：打印环形文件的元信息。

use std::fmt::Write as _;
use std::io;
use std::path::Path;

use crate::engine::Reader;
use humansize::{BINARY, format_size};

use super::report;

pub fn run(path: &Path) -> io::Result<()> {
    let reader = Reader::open(path)?;
    let (header, file_len) = reader.with_snapshot(|r, h| Ok((*h, r.file_len()?)))?;
    let used = header.total_written.min(header.capacity);
    let mut text = String::new();
    let _ = writeln!(text, "文件:       {}", path.display());
    let _ = writeln!(
        text,
        "文件大小:   {} 字节（{}）",
        file_len,
        format_size(file_len, BINARY)
    );
    let _ = writeln!(
        text,
        "数据区容量: {} 字节（{}）",
        header.capacity,
        format_size(header.capacity, BINARY)
    );
    let _ = writeln!(text, "写位置:     {} 字节", header.write_pos);
    let _ = writeln!(text, "累计写入:   {} 字节", header.total_written);
    let _ = writeln!(
        text,
        "有效数据:   {} 字节（{}）",
        used,
        format_size(used, BINARY)
    );
    let _ = writeln!(text, "头部序号:   {}", header.seq);
    report::stdout(&text);
    Ok(())
}
