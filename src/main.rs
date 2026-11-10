//! ringlog 命令行入口。

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ringlog::cli::{self, Cli, Mode};
use ringlog::follow;
use ringlog::input::LineSplitter;
use ringlog::ring::{Reader, Ring};

/// 输入读取线程每次向主线程投递的块大小。
const INPUT_CHUNK: usize = 256 * 1024;
/// 攒满这么多输入字节就落一批（高吞吐路径）。
const FLUSH_BYTES: usize = 256 * 1024;
/// 攒满这么多条记录就落一批。
const FLUSH_RECORDS: usize = 4096;
/// 低速率输入的兜底落盘间隔：保证跟随端最多滞后这么久就能看到数据。
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args
        .iter()
        .any(|a| matches!(a.to_str(), Some("-h" | "--help")))
    {
        write_stdout(cli::HELP);
        return ExitCode::SUCCESS;
    }
    if args
        .iter()
        .any(|a| matches!(a.to_str(), Some("-V" | "--version")))
    {
        write_stdout(&format!("ringlog {}\n", env!("CARGO_PKG_VERSION")));
        return ExitCode::SUCCESS;
    }

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        // 下游（head、grep -m 等）提前关闭管道是正常用法，安静退出。
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            write_stderr(&format!("ringlog: {e}\n"));
            match e.kind() {
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => ExitCode::from(2),
                _ => ExitCode::from(1),
            }
        }
    }
}

/// 写 stdout；下游管道关闭或输出失败时忽略，避免管道场景下 panic。
fn write_stdout(text: &str) {
    let _ = io::stdout().write_all(text.as_bytes());
}

/// 写 stderr；stderr 被关闭时忽略。
fn write_stderr(text: &str) {
    let _ = io::stderr().write_all(text.as_bytes());
}

fn run(args: Vec<OsString>) -> io::Result<()> {
    let cli = cli::parse(args).map_err(|msg| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{msg}（用 --help 查看用法）"),
        )
    })?;
    match cli.mode {
        Mode::Write => write_mode(&cli),
        Mode::Read => {
            let reader = Reader::open(&cli.path)?;
            follow::run(&reader, cli.follow, cli.lines, cli.interval)
        }
        Mode::Info => info_mode(&cli.path),
    }
}

fn write_mode(cli: &Cli) -> io::Result<()> {
    let size = cli
        .size
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "写入模式缺少 -s"))?;
    let mut ring = Ring::open(&cli.path, size, cli.force, cli.sync)?;

    // stdin 由独立线程读取，主线程按「攒满一批」或「100ms 超时」落盘。
    // 这样低速率输入最多滞后约 100ms 就能被跟随端看到，而不是等到 EOF。
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
    // 持续但低速率的数据流可能永远不会出现 100ms 的空闲。
    let mut last_flush = Instant::now();
    let result = loop {
        let wait = FLUSH_INTERVAL.saturating_sub(last_flush.elapsed());
        match receiver.recv_timeout(wait) {
            Ok(chunk) => {
                splitter.feed(&chunk);
                account_truncation(&mut splitter, &mut truncated_lines, &mut warned_truncation);
                if let Err(e) = flush_batch(&mut ring, &mut splitter, false) {
                    break Err(e);
                }
                if last_flush.elapsed() >= FLUSH_INTERVAL
                    && let Err(e) = flush_batch(&mut ring, &mut splitter, true)
                {
                    break Err(e);
                }
            }
            // 低速率输入的兜底：把已攒下的完整记录落盘。
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(e) = flush_batch(&mut ring, &mut splitter, true) {
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

    splitter.finish();
    account_truncation(&mut splitter, &mut truncated_lines, &mut warned_truncation);
    flush_batch(&mut ring, &mut splitter, true)?;

    if truncated_lines > 0 {
        write_stderr(&format!(
            "ringlog: 警告：共 {truncated_lines} 行过长，已截断（每行最多保留 {} 字节，含换行）\n",
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
        write_stderr("ringlog: 警告：检测到超长行，已截断保留行首\n");
        *warned = true;
    }
}

fn flush_batch(ring: &mut Ring, splitter: &mut LineSplitter, force: bool) -> io::Result<()> {
    if !force && splitter.payload_len() < FLUSH_BYTES && splitter.records_len() < FLUSH_RECORDS {
        return Ok(());
    }
    let (payload, records) = splitter.take();
    ring.append(&payload, &records)
}

fn info_mode(path: &Path) -> io::Result<()> {
    let reader = Reader::open(path)?;
    let (header, file_len) = reader.with_snapshot(|r, h| Ok((*h, r.file_len()?)))?;
    let used = header.total_written.min(header.capacity);
    let mut text = String::new();
    let _ = writeln!(text, "文件:       {}", path.display());
    let _ = writeln!(
        text,
        "文件大小:   {} 字节（{}）",
        file_len,
        human_size(file_len)
    );
    let _ = writeln!(
        text,
        "数据区容量: {} 字节（{}）",
        header.capacity,
        human_size(header.capacity)
    );
    let _ = writeln!(text, "写位置:     {} 字节", header.write_pos);
    let _ = writeln!(text, "累计写入:   {} 字节", header.total_written);
    let _ = writeln!(text, "有效数据:   {} 字节（{}）", used, human_size(used));
    let _ = writeln!(text, "头部序号:   {}", header.seq);
    let _ = writeln!(text, "重置纪元:   {}", header.epoch);
    write_stdout(&text);
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
