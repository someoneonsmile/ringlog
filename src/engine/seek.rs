//! 首屏定位：在环形窗口里找一个「记录起点」作为读取起始位置。
//!
//! 数据区任意时刻都是一串以 `\n` 结尾的完整记录，因此定位只需要扫描换行符：
//! - 窗口起点（`total_written - capacity`）可能落在一条记录中间。它前一个字节已被
//!   新数据覆盖，无法判断是否对齐，所以整条丢弃、从下一个换行符之后开始；
//! - 窗口起点恰好落在物理偏移 0（即记录边界）时不能丢第一条记录。
//!
//! 这里只报告「起点和丢了什么」，不打印任何东西——是否告警由调用方决定。

use std::io;

use super::format::Header;
use super::ring::Reader;

/// 每次扫描最多搬运的字节数。
const CHUNK: usize = 64 * 1024;

/// 首屏从哪开始输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPos {
    /// 输出全部有效数据。
    Oldest,
    /// 跳过历史，只输出之后的新数据。
    Newest,
    /// 输出最近 `n` 行；有效数据不足 `n` 行时输出全部。
    Last(u64),
}

/// 窗口定位结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    /// 记录起点的逻辑偏移。
    pub pos: u64,
    /// 为对齐记录边界而丢弃的字节数（正常数据下最多一条记录）。
    pub dropped: u64,
    /// 窗口内没有任何换行符（数据异常），此时 `dropped` 是全部可读数据。
    pub no_newline: bool,
}

impl Located {
    fn at(pos: u64) -> Self {
        Self {
            pos,
            dropped: 0,
            no_newline: false,
        }
    }
}

/// 计算首屏起点（逻辑偏移 = 累计写入字节数）。
pub fn locate_start(reader: &Reader, header: &Header, start: StartPos) -> io::Result<Located> {
    let end = header.total_written;
    let oldest = end.saturating_sub(header.capacity);
    let aligned = oldest.is_multiple_of(header.capacity);
    match start {
        StartPos::Oldest => {
            if aligned {
                Ok(Located::at(oldest))
            } else {
                scan_to_next_line(reader, oldest, end)
            }
        }
        StartPos::Newest => Ok(Located::at(end)),
        StartPos::Last(n) => match scan_back(reader, end, oldest, n)? {
            // scan_back 返回的是换行符之后的位置，天然对齐，直接使用。
            Some(pos) => Ok(Located::at(pos)),
            // 窗口内不足 n 行：从最旧数据开始，但起点可能落在半条记录上。
            None if aligned => Ok(Located::at(oldest)),
            None => scan_to_next_line(reader, oldest, end),
        },
    }
}

/// 从 `from` 之后扫到第一个换行符（返回其后的位置），丢弃 `from` 处不完整的记录。
///
/// 环形窗口的起点（`total_written - capacity`）可能落在一条记录中间，
/// 而且它前一个字节已被新数据覆盖，无法据此判断是否对齐，所以只能向前扫到换行符。
/// 代价：接缝处最旧的一条记录可能被整体丢弃，但绝不会输出撕裂的半条记录。
pub fn scan_to_next_line(reader: &Reader, from: u64, upto: u64) -> io::Result<Located> {
    if from >= upto {
        return Ok(Located::at(from));
    }
    let mut buf = vec![0u8; CHUNK];
    let mut abs = from;
    while abs < upto {
        let n = ((upto - abs) as usize).min(CHUNK);
        reader.read_exact_abs(abs, &mut buf[..n])?;
        if let Some(i) = memchr::memchr(b'\n', &buf[..n]) {
            let pos = abs + i as u64 + 1;
            return Ok(Located {
                pos,
                dropped: pos - from,
                no_newline: false,
            });
        }
        abs += n as u64;
    }
    Ok(Located {
        pos: upto,
        dropped: upto - from,
        no_newline: true,
    })
}

/// 从末尾向前数 `n + 1` 个换行符，返回最后一个换行符之后的位置（即最后 `n` 行的起点）；
/// 窗口内凑不够 `n` 行时返回 `None`。
fn scan_back(reader: &Reader, end: u64, oldest: u64, n: u64) -> io::Result<Option<u64>> {
    let mut buf = vec![0u8; CHUNK];
    let mut abs = end;
    let mut seen = 0u64;
    // n = u64::MAX 时 n + 1 会溢出；饱和后永远凑不满，等价于「全部有效数据」。
    let target = n.saturating_add(1);
    while abs > oldest {
        let chunk = ((abs - oldest) as usize).min(CHUNK);
        let start = abs - chunk as u64;
        reader.read_exact_abs(start, &mut buf[..chunk])?;
        let mut slice = &buf[..chunk];
        while let Some(i) = memchr::memrchr(b'\n', slice) {
            seen += 1;
            if seen == target {
                return Ok(Some(start + i as u64 + 1));
            }
            slice = &slice[..i];
        }
        abs = start;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::super::format::file_size;
    use super::super::input::LineSplitter;
    use super::super::ring::Ring;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 造一个内容确定的环形文件，并在 Drop 时清理。
    struct TempRing {
        path: PathBuf,
    }

    impl TempRing {
        fn new(capacity: u64, data: &[u8]) -> (Self, Reader) {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ringlog-seek-{}-{id}.ring", std::process::id()));
            let mut ring = Ring::open(&path, file_size(capacity), false).unwrap();
            let mut splitter = LineSplitter::new(capacity);
            splitter.feed(data);
            ring.append(splitter.take(true)).unwrap();
            drop(ring);
            let reader = Reader::open(&path).unwrap();
            (Self { path }, reader)
        }
    }

    impl Drop for TempRing {
        fn drop(&mut self) {
            if let Ok(lock) = super::super::ring::lock_path(&self.path) {
                let _ = std::fs::remove_file(lock);
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn locate(reader: &Reader, pos: StartPos) -> Located {
        reader
            .with_snapshot(|r, h| locate_start(r, h, pos))
            .unwrap()
    }

    /// 一条正好 `capacity` 字节的记录（末字节为换行），内容用 `fill` 填充。
    fn full_record(capacity: u64, fill: u8) -> Vec<u8> {
        let mut line = vec![fill; capacity as usize];
        *line.last_mut().unwrap() = b'\n';
        line
    }

    #[test]
    fn oldest_without_wrap_is_zero() {
        let (_temp, reader) = TempRing::new(4096, b"a\nbb\nccc\n");
        assert_eq!(locate(&reader, StartPos::Oldest), Located::at(0));
        assert_eq!(locate(&reader, StartPos::Newest), Located::at(9));
        assert_eq!(locate(&reader, StartPos::Last(2)), Located::at(2));
        assert_eq!(locate(&reader, StartPos::Last(100)), Located::at(0));
    }

    #[test]
    fn last_u64_max_means_all_data() {
        let (_temp, reader) = TempRing::new(4096, b"a\nbb\nccc\n");
        assert_eq!(locate(&reader, StartPos::Last(u64::MAX)), Located::at(0));
    }

    #[test]
    fn oldest_at_exact_record_boundary_keeps_first_record() {
        // 两条正好占满数据区的记录：窗口起点落在物理偏移 0（记录边界），不能丢记录。
        let mut data = full_record(4096, b'A');
        data.extend_from_slice(&full_record(4096, b'B'));
        let (_temp, reader) = TempRing::new(4096, &data);
        assert_eq!(locate(&reader, StartPos::Oldest), Located::at(4096));
        assert_eq!(locate(&reader, StartPos::Last(1)), Located::at(4096));
        assert_eq!(locate(&reader, StartPos::Newest), Located::at(8192));
    }

    #[test]
    fn last_n_scans_back_across_multiple_chunks() {
        // 64 条 4096 字节记录正好绕两圈；数据区容量 128 KiB 只装得下最后 32 条。
        // 最后 20 行的起点距末尾 80 KiB，超过 scan_back 单次 64 KiB 的搬运窗口。
        let mut data = Vec::new();
        for _ in 0..64 {
            data.extend_from_slice(&full_record(4096, b'A'));
        }
        let (_temp, reader) = TempRing::new(131072, &data);
        assert_eq!(locate(&reader, StartPos::Last(20)), Located::at(180224));
        // 超过窗口内行数时回退到最旧数据（此处对齐，一条不丢）。
        assert_eq!(locate(&reader, StartPos::Last(40)), Located::at(131072));
        assert_eq!(locate(&reader, StartPos::Oldest), Located::at(131072));
    }

    #[test]
    fn oldest_skips_partial_record_at_seam() {
        // 记录依次占 [0,4096) [4096,5096) [填充到 8192) [8192,12288) [12288,13288)，
        // 窗口起点 9192 落在第三条记录内部 → 整条丢弃，从 12288 继续。
        let mut data = full_record(4096, b'A');
        data.resize(data.len() + 999, b'B');
        data.push(b'\n');
        data.extend_from_slice(&full_record(4096, b'C'));
        data.resize(data.len() + 999, b'D');
        data.push(b'\n');

        let (_temp, reader) = TempRing::new(4096, &data);
        assert_eq!(
            locate(&reader, StartPos::Oldest),
            Located {
                pos: 12288,
                dropped: 3096,
                no_newline: false,
            }
        );
        assert_eq!(locate(&reader, StartPos::Last(1)), Located::at(12288));
        assert_eq!(locate(&reader, StartPos::Last(100)).pos, 12288);
        assert_eq!(locate(&reader, StartPos::Newest), Located::at(13288));
    }
}
