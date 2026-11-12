//! 读取与跟随（`-f` / `-n`）。
//!
//! 每次 `step` 在 [`Reader::with_snapshot`] 的共享锁内只做两件事：读头部、把一个
//! chunk 的数据拷到内存。锁释放之后才写 stdout，所以下游管道变慢也不会阻塞写者。
//!
//! 跟随语义类似 `tail -f`：
//! - 先输出有效数据中最近的部分（`-n` 控制行数，缺省为全部）；
//! - 之后持续输出新增记录；
//! - 读者落后超过数据区容量时，旧数据已被覆盖：告警并跳至最旧可用数据，
//!   从下一个换行符开始继续，丢弃可能不完整的记录；
//! - 文件被外部截断并重新初始化（累计写入回退）时告警并重新定位。

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ringlog::ring::Reader;
use ringlog::seek::{StartPos, locate_start, scan_to_next_line};

use super::report;

/// 每次加锁最多搬运的字节数。
const CHUNK: usize = 64 * 1024;
/// “跟读落后”告警的最小间隔，避免下游慢时刷屏。
const WARN_INTERVAL: Duration = Duration::from_secs(1);

/// 按 `start` 决定首屏起点，`follow` 为真时持续输出新数据。
pub fn run(reader: &Reader, follow: bool, start: StartPos, interval: Duration) -> io::Result<()> {
    let mut tailer = Tailer::new(reader, report::buffered_stdout(), start);
    tailer.init()?;

    if !follow {
        // 一次性输出：读到 init 之后看到的末尾为止。
        let target = reader.with_snapshot(|_, h| Ok(h.total_written))?;
        while tailer.consumed < target {
            tailer.step()?;
        }
        return tailer.flush();
    }

    loop {
        let touched = tailer.step()?;
        tailer.flush()?;
        if !touched {
            std::thread::sleep(interval);
        }
    }
}

struct Tailer<'a, W: Write> {
    reader: &'a Reader,
    out: W,
    /// 已消费到的逻辑位置（= 累计写入字节数）。
    consumed: u64,
    /// 已消费但尚未构成完整记录（没有换行符）的尾部字节。
    pending: Vec<u8>,
    start: StartPos,
    /// 上次打印“落后丢数据”告警之后累计丢失的字节数（避免告警刷屏）。
    lost_bytes: u64,
    last_warn: Option<Instant>,
}

#[derive(Clone, Copy)]
enum Warning {
    /// 落后超过一整个数据区：`bytes` 为累计丢失量。
    Lost(u64),
    /// 文件被重新初始化（累计写入回退）。
    Reset,
}

impl<'a, W: Write> Tailer<'a, W> {
    fn new(reader: &'a Reader, out: W, start: StartPos) -> Self {
        Self {
            reader,
            out,
            consumed: 0,
            pending: Vec::new(),
            start,
            lost_bytes: 0,
            last_warn: None,
        }
    }

    fn init(&mut self) -> io::Result<()> {
        let located = self
            .reader
            .with_snapshot(|r, h| locate_start(r, h, self.start))?;
        if located.no_newline {
            warn_no_newline(located.dropped);
        }
        self.consumed = located.pos;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    /// 读一小块并输出。返回是否有新数据被输出。
    ///
    /// 锁内只做「读头部 + 拷贝一块数据」；写 stdout 在锁外完成。
    fn step(&mut self) -> io::Result<bool> {
        let start = self.consumed;
        let mut buf = vec![0u8; CHUNK];
        let (consumed, read_len, warning, dropped_no_newline) =
            self.reader.with_snapshot(|r, h| {
                let end = h.total_written;
                // 累计写入回退说明文件被外部截断并重新初始化。
                if end < start {
                    // 重置后的内容都算“新数据”，从头输出有效部分。
                    let located = locate_start(r, h, StartPos::Oldest)?;
                    let dropped = located.no_newline.then_some(located.dropped);
                    return Ok((located.pos, 0, Some(Warning::Reset), dropped));
                }
                let mut available = end - start;
                let mut consumed = start;
                let mut warning = None;
                let mut dropped = None;
                if available > h.capacity {
                    // 落后超过整个环形窗口：旧数据已被覆盖。
                    let oldest = end - h.capacity;
                    let lost = available - h.capacity;
                    consumed = if oldest.is_multiple_of(h.capacity) {
                        oldest
                    } else {
                        let located = scan_to_next_line(r, oldest, end)?;
                        dropped = located.no_newline.then_some(located.dropped);
                        located.pos
                    };
                    available = end - consumed;
                    warning = Some(Warning::Lost(lost));
                }
                if available == 0 {
                    return Ok((consumed, 0, warning, dropped));
                }
                let n = available.min(CHUNK as u64) as usize;
                r.read_exact_abs(consumed, &mut buf[..n])?;
                Ok((consumed, n, warning, dropped))
            })?;
        if let Some(dropped) = dropped_no_newline {
            warn_no_newline(dropped);
        }

        match warning {
            Some(Warning::Lost(bytes)) => {
                self.lost_bytes += bytes;
                let due = self.last_warn.is_none_or(|t| t.elapsed() >= WARN_INTERVAL);
                if due {
                    report::warn(format_args!(
                        "警告：跟读落后，累计约 {} 字节旧数据已被覆盖，跳到最旧数据继续",
                        self.lost_bytes
                    ));
                    self.lost_bytes = 0;
                    self.last_warn = Some(Instant::now());
                }
            }
            Some(Warning::Reset) => {
                report::warn("警告：文件已被重新初始化，从当前有效数据继续");
                self.pending.clear();
                self.consumed = consumed;
                return Ok(true);
            }
            None => {}
        }

        self.consumed = consumed + read_len as u64;
        if read_len > 0 {
            self.write_records(&buf[..read_len])?;
        }
        Ok(read_len > 0)
    }

    /// 只输出完整记录，半条记录留在 `pending` 里等后续数据。
    fn write_records(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(chunk);
        if let Some(i) = memchr::memrchr(b'\n', &self.pending) {
            self.out.write_all(&self.pending[..=i])?;
            self.pending.drain(..=i);
        }
        // 正常数据里单条记录不会超过数据区容量；超过说明数据异常，避免内存无限增长。
        if self.pending.len() as u64 > self.reader.capacity() {
            report::warn(format_args!(
                "警告：{} 字节数据中没有换行符，丢弃",
                self.pending.len()
            ));
            self.pending.clear();
        }
        Ok(())
    }
}

fn warn_no_newline(dropped: u64) {
    report::warn(format_args!(
        "警告：可读数据中没有换行符，丢弃 {dropped} 字节"
    ));
}
