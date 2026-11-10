//! 写入模式：stdin 管道 → 环形文件。
//!
//! stdin 由独立线程读取，主线程按「攒满一批」或「100 ms 超时」落盘，
//! 这样低速率输入最多滞后约 100 ms 就能被跟随端看到，而不是等到 EOF。

use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ringlog::input::LineSplitter;
use ringlog::ring::Ring;

use super::report;

/// 输入读取线程每次向主线程投递的块大小。
const INPUT_CHUNK: usize = 256 * 1024;
/// 攒满这么多输入字节就落一批（高吞吐路径）。
const FLUSH_BYTES: usize = 256 * 1024;
/// 攒满这么多条记录就落一批。
const FLUSH_RECORDS: usize = 4096;
/// 低速率输入的兜底落盘间隔：保证跟随端最多滞后这么久就能看到数据。
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

pub fn run(path: &Path, size: u64, force: bool, sync: bool) -> io::Result<()> {
    let mut ring = Ring::open(path, size, force, sync)?;

    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(4);
    let reader_thread = thread::spawn(move || -> io::Result<()> {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut buf = vec![0u8; INPUT_CHUNK];
        loop {
            match input.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    if sender.send(buf[..n].to_vec()).is_err() {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    });

    let mut splitter = LineSplitter::new(ring.capacity());
    let mut truncated_lines: u64 = 0;
    let mut warned_truncation = false;
    // 注意按「距上次落盘的时间」计时，而不是等输入空闲：
    // 持续但低速率的数据流可能永远不会出现 100 ms 的空闲。
    let mut last_flush = Instant::now();
    let result = loop {
        let wait = FLUSH_INTERVAL.saturating_sub(last_flush.elapsed());
        match receiver.recv_timeout(wait) {
            Ok(chunk) => {
                splitter.feed(&chunk);
                account_truncation(&mut splitter, &mut truncated_lines, &mut warned_truncation);
                if let Err(e) = flush_batch(&mut ring, &mut splitter, false, false) {
                    break Err(e);
                }
                if last_flush.elapsed() >= FLUSH_INTERVAL
                    && let Err(e) = flush_batch(&mut ring, &mut splitter, true, false)
                {
                    break Err(e);
                }
            }
            // 低速率输入的兜底：把已攒下的完整记录落盘。
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(e) = flush_batch(&mut ring, &mut splitter, true, false) {
                    break Err(e);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
        }
        if last_flush.elapsed() >= FLUSH_INTERVAL {
            last_flush = Instant::now();
        }
    };

    // 让可能阻塞在 send 上的读线程退出，再收尾。
    drop(receiver);
    let reader_result = reader_thread
        .join()
        .map_err(|_| io::Error::other("输入读取线程异常退出"))?;

    result?;
    reader_result?;

    // 输入已结束：把没有换行符的最后一行补上 `\n` 一起落盘。
    flush_batch(&mut ring, &mut splitter, true, true)?;
    account_truncation(&mut splitter, &mut truncated_lines, &mut warned_truncation);

    if truncated_lines > 0 {
        report::warn(format_args!(
            "警告：共 {truncated_lines} 行过长，已截断（每行最多保留 {} 字节，含换行）",
            splitter.max_record()
        ));
    }
    Ok(())
}

fn account_truncation(splitter: &mut LineSplitter, total: &mut u64, warned: &mut bool) {
    let count = splitter.take_truncated_lines();
    if count == 0 {
        return;
    }
    *total += count;
    if !*warned {
        report::warn("警告：检测到超长行，已截断保留行首");
        *warned = true;
    }
}

fn flush_batch(
    ring: &mut Ring,
    splitter: &mut LineSplitter,
    force: bool,
    final_chunk: bool,
) -> io::Result<()> {
    if !force && splitter.payload_len() < FLUSH_BYTES && splitter.records_len() < FLUSH_RECORDS {
        return Ok(());
    }
    ring.append(splitter.take(final_chunk))
}
