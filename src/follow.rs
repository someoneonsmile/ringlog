//! 读取与跟随（`-f`）。
//!
//! 每次 `step` 在 [`Reader::with_snapshot`] 的共享锁内只做两件事：读头部、把一个
//! chunk 的数据拷到内存。锁释放之后才写 stdout，所以下游管道变慢也不会阻塞写者。
//!
//! 跟随语义类似 `tail -f`：
//! - 先输出有效数据中最近的部分（`-n` 控制行数，缺省为全部）；
//! - 之后持续输出新增记录；
//! - 读者落后超过数据区容量时，旧数据已被覆盖：告警并跳至最旧可用数据，
//!   从下一个换行符开始继续，丢弃可能不完整的记录；
//! - 文件被 `--force` 重建（头部的 `epoch` 变化）时告警并重新定位。

use std::io::{self, BufWriter, Write};
use std::time::{Duration, Instant};

use crate::format::Header;
use crate::ring::Reader;

/// 每次加锁最多搬运的字节数。
const CHUNK: usize = 64 * 1024;
/// “跟读落后”告警的最小间隔，避免下游慢时刷屏。
const WARN_INTERVAL: Duration = Duration::from_secs(1);

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

/// 按 `start` 决定首屏起点，`follow` 为真时持续输出新数据。
pub fn run(
    reader: &Reader,
    follow: bool,
    start: StartPos,
    interval: Duration,
) -> io::Result<()> {
    let stdout = io::stdout();
    let mut tailer = Tailer::new(
        reader,
        BufWriter::with_capacity(CHUNK, stdout.lock()),
        start,
    );
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
    /// 当前跟随的“重置纪元”，与头部不一致说明文件被 `--force` 重建过。
    epoch: u64,
    /// 上次打印“落后丢数据”告警之后累计丢失的字节数（避免告警刷屏）。
    lost_bytes: u64,
    last_warn: Option<Instant>,
}

#[derive(Clone, Copy)]
enum Warning {
    Lost(u64),
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
            epoch: 0,
            lost_bytes: 0,
            last_warn: None,
        }
    }

    fn init(&mut self) -> io::Result<()> {
        let (consumed, epoch) = self
            .reader
            .with_snapshot(|r, h| Ok((locate_start(r, h, self.start)?, h.epoch)))?;
        self.consumed = consumed;
        self.epoch = epoch;
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
        let expected_epoch = self.epoch;
        let mut buf = vec![0u8; CHUNK];
        let (consumed, read_len, warning, new_epoch) = self.reader.with_snapshot(|r, h| {
            let end = h.total_written;
            // 纪元变化说明文件被 --force 重建；total_written 回退是兜底判断。
            if h.epoch != expected_epoch || end < start {
                // 重建后的内容都算“新数据”，从头输出有效部分。
                let consumed = locate_start(r, h, StartPos::Oldest)?;
                return Ok((consumed, 0, Some(Warning::Reset), Some(h.epoch)));
            }
            let mut available = end - start;
            let mut consumed = start;
            let mut warning = None;
            if available > h.capacity {
                // 落后超过整个环形窗口：旧数据已被覆盖。
                let oldest = end - h.capacity;
                let lost = available - h.capacity;
                consumed = if oldest.is_multiple_of(h.capacity) {
                    oldest
                } else {
                    scan_to_next_line(r, oldest, end)?
                };
                available = end - consumed;
                warning = Some(Warning::Lost(lost));
            }
            if available == 0 {
                return Ok((consumed, 0, warning, None));
            }
            let n = available.min(CHUNK as u64) as usize;
            r.read_exact_abs(consumed, &mut buf[..n])?;
            Ok((consumed, n, warning, None))
        })?;
        if let Some(epoch) = new_epoch {
            self.epoch = epoch;
        }

        match warning {
            Some(Warning::Lost(bytes)) => {
                self.lost_bytes += bytes;
                let due = self.last_warn.is_none_or(|t| t.elapsed() >= WARN_INTERVAL);
                if due {
                    crate::warn(format_args!(
                        "警告：跟读落后，累计约 {} 字节旧数据已被覆盖，跳到最旧数据继续",
                        self.lost_bytes
                    ));
                    self.lost_bytes = 0;
                    self.last_warn = Some(Instant::now());
                }
            }
            Some(Warning::Reset) => {
                crate::warn("警告：文件已被重新初始化，从当前有效数据继续");
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
            crate::warn(format_args!(
                "警告：{} 字节数据中没有换行符，丢弃",
                self.pending.len()
            ));
            self.pending.clear();
        }
        Ok(())
    }
}

/// 计算首屏起点（逻辑偏移 = 累计写入字节数）。
fn locate_start(reader: &Reader, header: &Header, start: StartPos) -> io::Result<u64> {
    let end = header.total_written;
    let oldest = end.saturating_sub(header.capacity);
    // 物理偏移 0 处永远是记录起点：初始化后从这里开始写，每次绕回也是从 0 写新记录。
    // 因此窗口起点恰好落在 0 偏移时（oldest % capacity == 0）不能丢第一条记录。
    let aligned = oldest.is_multiple_of(header.capacity);
    match start {
        StartPos::Oldest => {
            if aligned {
                Ok(oldest)
            } else {
                scan_to_next_line(reader, oldest, end)
            }
        }
        StartPos::Newest => Ok(end),
        StartPos::Last(n) => match scan_back(reader, end, oldest, n)? {
            // scan_back 返回的是换行符之后的位置，天然对齐，直接使用。
            Some(start) => Ok(start),
            // 窗口内不足 n 行：从最旧数据开始，但起点可能落在半条记录上。
            None if aligned => Ok(oldest),
            None => scan_to_next_line(reader, oldest, end),
        },
    }
}

/// 从 `from` 之后扫到第一个换行符（返回其后的位置），丢弃 `from` 处不完整的记录。
///
/// 环形窗口的起点（`total_written - capacity`）可能落在一条记录中间，
/// 而且它前一个字节已被新数据覆盖，无法据此判断是否对齐，所以只能向前扫到换行符。
/// 代价：接缝处最旧的一条记录可能被整体丢弃，但绝不会输出撕裂的半条记录。
fn scan_to_next_line(reader: &Reader, from: u64, upto: u64) -> io::Result<u64> {
    if from >= upto {
        return Ok(from);
    }
    let mut buf = vec![0u8; CHUNK];
    let mut abs = from;
    while abs < upto {
        let n = ((upto - abs) as usize).min(CHUNK);
        reader.read_exact_abs(abs, &mut buf[..n])?;
        if let Some(i) = memchr::memchr(b'\n', &buf[..n]) {
            return Ok(abs + i as u64 + 1);
        }
        abs += n as u64;
    }
    // 可读窗口内没有换行符（数据异常）：全部丢弃，避免输出乱码。
    crate::warn(format_args!(
        "警告：可读数据中没有换行符，丢弃 {} 字节",
        upto - from
    ));
    Ok(upto)
}

/// 从末尾向前数 `n + 1` 个换行符，返回最后一个换行符之后的位置（即最后 `n` 行的起点）；
/// 窗口内凑不够 `n` 行时返回 `None`。
fn scan_back(reader: &Reader, end: u64, oldest: u64, n: u64) -> io::Result<Option<u64>> {
    let mut buf = vec![0u8; CHUNK];
    let mut abs = end;
    let mut seen = 0u64;
    // n = u64::MAX 时 n + 1 会溢出；饱和后永远凑不满，等价于“全部有效数据”。
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
