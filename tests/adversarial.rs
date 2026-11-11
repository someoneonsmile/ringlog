//! 对抗性黑盒测试：只通过命令行观察 ringlog 的对外行为。
//!
//! 覆盖极小容量边界、`-n` 边界、头部损坏、SIGKILL 续写、写者互斥、
//! EPIPE、并发压测、`--force` 重置、异常文件与参数错误。
//! 所有子进程都有超时或 Drop 清理，任何一条测试失败都不会把测试进程挂死。

mod common;

use common::{TestDir, bin, lock_path, path_text};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "等待超时: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn lock_held_by_other(lock: &Path) -> bool {
    File::open(lock).is_ok_and(|f| f.try_lock().is_err())
}

/// 等到写者已经持有侧车锁（而不是仅创建了锁文件）。
fn wait_writer_ready(file: &Path) {
    // 写者先创建数据文件再取锁，锁路径由规范路径决定：每轮重新求值，
    // 避免在数据文件/符号链接尚未出现时缓存了错误的路径。
    let mut lock = lock_path(file);
    wait_until(
        || {
            lock = lock_path(file);
            lock.exists()
        },
        Duration::from_secs(5),
        "锁文件出现",
    );
    wait_until(
        || lock_held_by_other(&lock),
        Duration::from_secs(5),
        "写者持有 flock",
    );
}

fn wait_status_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

struct Capture {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

impl Capture {
    fn code(&self) -> Option<i32> {
        self.status.code()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

fn pump(mut reader: impl Read, sink: Arc<Mutex<Vec<u8>>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// 等子进程退出（带超时），同时收集 stdout/stderr；`child.stdin` 可为 None。
fn wait_capture(mut child: Child, timeout: Duration) -> Capture {
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out_sink = Arc::new(Mutex::new(Vec::new()));
    let err_sink = Arc::new(Mutex::new(Vec::new()));
    let out_handle = {
        let sink = out_sink.clone();
        thread::spawn(move || pump(stdout, sink))
    };
    let err_handle = {
        let sink = err_sink.clone();
        thread::spawn(move || pump(stderr, sink))
    };

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().unwrap();
        }
        thread::sleep(Duration::from_millis(5));
    };
    let _ = out_handle.join();
    let _ = err_handle.join();
    let stdout = out_sink.lock().unwrap().clone();
    let stderr = err_sink.lock().unwrap().clone();
    Capture {
        status,
        stdout,
        stderr,
        timed_out,
    }
}

fn run(args: &[&str]) -> Capture {
    let mut cmd = bin();
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    wait_capture(cmd.spawn().unwrap(), Duration::from_secs(10))
}

fn write_with(file: &Path, extra: &[&str], input: &[u8]) -> Capture {
    let mut cmd = bin();
    cmd.args(extra)
        .arg(file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_vec();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let cap = wait_capture(child, Duration::from_secs(20));
    writer.join().unwrap();
    cap
}

fn write_stdin(file: &Path, size: &str, input: &[u8]) -> Capture {
    write_with(file, &["-s", size], input)
}

fn write_stdin_force(file: &Path, size: &str, input: &[u8]) -> Capture {
    write_with(file, &["-s", size, "--force"], input)
}

/// 分批写入并等待每一批真正落盘（头部 seq 增加），用于确定性地制造多次 append。
fn write_paced(file: &Path, size: &str, groups: &[Vec<u8>]) -> Capture {
    let mut cmd = bin();
    cmd.args(["-s", size])
        .arg(file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for (idx, group) in groups.iter().enumerate() {
        stdin.write_all(group).unwrap();
        if idx + 1 < groups.len() {
            let expected = idx as u64 + 2;
            wait_until(
                || max_seq(file) == Some(expected),
                Duration::from_secs(5),
                "本批写入落盘",
            );
        }
    }
    drop(stdin);
    wait_capture(child, Duration::from_secs(20))
}

/// 一次 dump 的有效数据（`-n` 足够大）。
fn dump(file: &Path) -> Vec<u8> {
    let out = run(&["-n", "100000", path_text(file)]);
    assert_eq!(out.code(), Some(0), "dump 失败: {}", out.err());
    assert!(!out.timed_out, "dump 超时");
    out.stdout
}

/// 按换行切分；数据以 `\n` 结尾，因此去掉最后的空段。中间的空白行保留。
fn records(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    assert!(data.ends_with(b"\n"), "输出没有以换行结尾");
    let mut parts: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    parts.pop();
    parts
}

fn assert_no_panic(stderr: &str) {
    assert!(!stderr.contains("panicked"), "子进程 panic: {stderr}");
    assert!(!stderr.contains("RUST_BACKTRACE"), "子进程 panic: {stderr}");
}

// ---------------------------------------------------------------------------
// 跟随读者
// ---------------------------------------------------------------------------

struct Follower {
    child: Option<Child>,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    pumps: Option<Vec<thread::JoinHandle<()>>>,
}

impl Follower {
    fn spawn(file: &Path, extra: &[&str]) -> Self {
        let mut cmd = bin();
        cmd.arg("-f")
            .args(extra)
            .arg(file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let pumps = vec![
            {
                let sink = out.clone();
                thread::spawn(move || pump(stdout, sink))
            },
            {
                let sink = err.clone();
                thread::spawn(move || pump(stderr, sink))
            },
        ];
        Self {
            child: Some(child),
            out,
            err,
            pumps: Some(pumps),
        }
    }

    fn stdout(&self) -> Vec<u8> {
        self.out.lock().unwrap().clone()
    }

    fn wait_out(&self, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let data = self.out.lock().unwrap();
                if data.windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_stdout_len(&self, len: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.out.lock().unwrap().len() >= len {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn try_status(&mut self) -> Option<ExitStatus> {
        self.child.as_mut().unwrap().try_wait().unwrap()
    }

    fn stop(mut self) -> (Vec<u8>, Vec<u8>, Option<i32>) {
        let mut child = self.child.take().unwrap();
        let _ = child.kill();
        let status = child.wait().unwrap();
        if let Some(pumps) = self.pumps.take() {
            for pump in pumps {
                let _ = pump.join();
            }
        }
        let stderr = self.err.lock().unwrap().clone();
        (self.stdout(), stderr, status.code())
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// 长驻写者守卫
// ---------------------------------------------------------------------------

struct GuardChild {
    child: Option<Child>,
}

impl GuardChild {
    fn spawn_writer(file: &Path, size: &str) -> Self {
        let mut cmd = bin();
        cmd.args(["-s", size])
            .arg(file)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Self {
            child: Some(cmd.spawn().unwrap()),
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }
}

impl Drop for GuardChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// 文件格式（测试侧独立实现，用来手工改坏/校验头部）
// ---------------------------------------------------------------------------

const SLOT_BYTES: usize = 64;

#[derive(Clone, Copy, Debug)]
struct Slot {
    seq: u64,
    epoch: u64,
    capacity: u64,
    write_pos: u64,
    total_written: u64,
}

fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0x82F6_3B78 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn parse_slot(buf: &[u8]) -> Option<Slot> {
    assert_eq!(buf.len(), SLOT_BYTES);
    let stored = u32::from_le_bytes(buf[56..60].try_into().unwrap());
    if crc32c(&buf[..56]) != stored {
        return None;
    }
    let magic = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    if magic != u64::from_le_bytes(*b"RINGLOG1") {
        return None;
    }
    if u32::from_le_bytes(buf[8..12].try_into().unwrap()) != 1 {
        return None;
    }
    if u32::from_le_bytes(buf[12..16].try_into().unwrap()) != SLOT_BYTES as u32 {
        return None;
    }
    let capacity = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let write_pos = u64::from_le_bytes(buf[24..32].try_into().unwrap());
    let total_written = u64::from_le_bytes(buf[32..40].try_into().unwrap());
    let seq = u64::from_le_bytes(buf[40..48].try_into().unwrap());
    let epoch = u64::from_le_bytes(buf[48..56].try_into().unwrap());
    if capacity == 0 || write_pos >= capacity || total_written % capacity != write_pos {
        return None;
    }
    Some(Slot {
        seq,
        epoch,
        capacity,
        write_pos,
        total_written,
    })
}

fn read_slots(file: &Path) -> [Option<Slot>; 2] {
    let bytes = fs::read(file).unwrap();
    assert!(bytes.len() >= 2 * SLOT_BYTES, "文件没有完整头部");
    let mut slots = [None, None];
    for (idx, slot) in slots.iter_mut().enumerate() {
        *slot = parse_slot(&bytes[idx * SLOT_BYTES..(idx + 1) * SLOT_BYTES]);
    }
    slots
}

fn try_read_slots(file: &Path) -> [Option<Slot>; 2] {
    let Ok(bytes) = fs::read(file) else {
        return [None, None];
    };
    if bytes.len() < 2 * SLOT_BYTES {
        return [None, None];
    }
    let mut slots = [None, None];
    for (idx, slot) in slots.iter_mut().enumerate() {
        *slot = parse_slot(&bytes[idx * SLOT_BYTES..(idx + 1) * SLOT_BYTES]);
    }
    slots
}

fn max_seq(file: &Path) -> Option<u64> {
    try_read_slots(file)
        .iter()
        .flatten()
        .map(|slot| slot.seq)
        .max()
}

fn build_slot(seq: u64, epoch: u64, capacity: u64, write_pos: u64, total: u64) -> [u8; 64] {
    let mut buf = [0u8; 64];
    buf[0..8].copy_from_slice(&u64::from_le_bytes(*b"RINGLOG1").to_le_bytes());
    buf[8..12].copy_from_slice(&1u32.to_le_bytes());
    buf[12..16].copy_from_slice(&(SLOT_BYTES as u32).to_le_bytes());
    buf[16..24].copy_from_slice(&capacity.to_le_bytes());
    buf[24..32].copy_from_slice(&write_pos.to_le_bytes());
    buf[32..40].copy_from_slice(&total.to_le_bytes());
    buf[40..48].copy_from_slice(&seq.to_le_bytes());
    buf[48..56].copy_from_slice(&epoch.to_le_bytes());
    let crc = crc32c(&buf[..56]);
    buf[56..60].copy_from_slice(&crc.to_le_bytes());
    buf
}

fn patch_slot(file: &Path, idx: usize, slot: &[u8; 64]) {
    let mut bytes = fs::read(file).unwrap();
    let start = idx * SLOT_BYTES;
    bytes[start..start + SLOT_BYTES].copy_from_slice(slot);
    fs::write(file, bytes).unwrap();
}

fn corrupt_slot_crc(file: &Path, idx: usize) {
    let mut bytes = fs::read(file).unwrap();
    bytes[idx * SLOT_BYTES] ^= 0xff;
    fs::write(file, bytes).unwrap();
}

// ---------------------------------------------------------------------------
// 行格式校验
// ---------------------------------------------------------------------------

/// 极小容量测试的合法记录：`T<RR>-<IIII>:` 后跟任意个 `z`。
fn tiny_line_ok(line: &[u8]) -> bool {
    if line.is_empty() {
        return true;
    }
    if line[0] != b'T' || line.len() < 9 || line[8] != b':' {
        return false;
    }
    line[1..3].iter().all(|b| b.is_ascii_digit())
        && line[3] == b'-'
        && line[4..8].iter().all(|b| b.is_ascii_digit())
        && line[9..].iter().all(|&b| b == b'z')
}

/// 并发压测的合法记录：`S<IIIIII>:` 后跟任意个 `x`。
fn stress_line_ok(line: &[u8]) -> bool {
    if line.is_empty() {
        return true;
    }
    if line[0] != b'S' || line.len() < 8 || line[7] != b':' {
        return false;
    }
    line[1..7].iter().all(|b| b.is_ascii_digit()) && line[8..].iter().all(|&b| b == b'x')
}

/// SIGKILL 压测的合法记录：`R<RR>-<IIIII>:` 后跟任意个 `k`。
fn crash_line_ok(line: &[u8]) -> bool {
    if line[0] != b'R' || line.len() < 10 || line[9] != b':' {
        return false;
    }
    line[1..3].iter().all(|b| b.is_ascii_digit())
        && line[3] == b'-'
        && line[4..9].iter().all(|b| b.is_ascii_digit())
        && line[10..].iter().all(|&b| b == b'k')
}

fn file_len(file: &Path) -> u64 {
    fs::metadata(file).unwrap().len()
}

// ---------------------------------------------------------------------------
// 1. 极小容量：8 KiB 文件 / 4096 B 数据区的边界记录
// ---------------------------------------------------------------------------

#[test]
fn tiny_ring_capacity_boundary_records() {
    let dir = TestDir::new("adv");

    // 恰好 4096 字节（4095 内容 + 换行）：占满整个数据区。
    let file = dir.file("exact.ring");
    let mut exact = vec![b'A'; 4095];
    exact.push(b'\n');
    let out = write_stdin(&file, "8K", &exact);
    assert_eq!(out.code(), Some(0), "写入失败: {}", out.err());
    assert_no_panic(&out.err());
    assert_eq!(file_len(&file), 8192, "文件大小必须恒定");
    assert_eq!(dump(&file), exact, "4096 字节记录应完整保留");

    // 4095 字节（4094 内容 + 换行）。
    let file = dir.file("under.ring");
    let mut under = vec![b'B'; 4094];
    under.push(b'\n');
    let out = write_stdin(&file, "8K", &under);
    assert_eq!(out.code(), Some(0), "写入失败: {}", out.err());
    assert_eq!(file_len(&file), 8192);
    assert_eq!(dump(&file), under, "4095 字节记录应完整保留");

    // 4097 字节：应截断为 4095 内容 + 换行，并告警。
    let file = dir.file("over.ring");
    let mut over = vec![b'C'; 4096];
    over.push(b'\n');
    let out = write_stdin(&file, "8K", &over);
    assert_eq!(out.code(), Some(0), "写入失败: {}", out.err());
    let stderr = out.err();
    assert_no_panic(&stderr);
    assert!(stderr.contains("截断"), "超长行应告警，stderr={stderr}");
    assert_eq!(file_len(&file), 8192);
    let data = dump(&file);
    let mut expected = vec![b'C'; 4095];
    expected.push(b'\n');
    assert_eq!(data, expected, "超长记录应保留行首且以换行结尾");
}

#[test]
fn tiny_ring_mixed_wrap_stays_consistent() {
    let dir = TestDir::new("adv");
    let file = dir.file("mixed.ring");

    for round in 0..4u32 {
        let mut input = String::new();
        for i in 0..300u32 {
            let fill = match i % 17 {
                0 => 4086, // 记录恰好 4096 字节
                3 => 4085, // 记录 4095 字节
                5 => 5000, // 超长，截断
                _ => 10 + (i % 50) as usize,
            };
            input.push_str(&format!("T{round:02}-{i:04}:"));
            input.extend(std::iter::repeat_n('z', fill));
            input.push('\n');
        }
        let out = write_stdin(&file, "8K", input.as_bytes());
        assert_eq!(out.code(), Some(0), "第 {round} 轮写入失败: {}", out.err());
        assert_no_panic(&out.err());
        assert_eq!(file_len(&file), 8192, "第 {round} 轮后文件大小必须恒定");
    }

    // 最后一批 200 条短记录，保证最后 3 条在逻辑上连续（最多一次环绕填充）。
    let guard: String = (0..200u32).map(|i| format!("T99-{i:04}:z\n")).collect();
    let out = write_stdin(&file, "8K", guard.as_bytes());
    assert_eq!(out.code(), Some(0), "guard 写入失败: {}", out.err());
    assert_eq!(file_len(&file), 8192);

    let data = dump(&file);
    let lines = records(&data);
    assert!(!lines.is_empty(), "dump 不应为空");
    for line in &lines {
        assert!(
            tiny_line_ok(line),
            "出现撕裂/非法记录: {:?}",
            String::from_utf8_lossy(&line[..line.len().min(80)])
        );
    }

    let expected_tail = b"T99-0197:z\nT99-0198:z\nT99-0199:z\n";
    assert!(data.ends_with(expected_tail), "最后 3 条记录不对");

    let out = run(&["-n", "3", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "{}", out.err());
    assert_eq!(out.stdout, expected_tail, "-n 3 应精确等于最后 3 行");
    let out = run(&["-n", "1", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "{}", out.err());
    assert_eq!(out.stdout, b"T99-0199:z\n", "-n 1 应精确等于最后 1 行");
}

// ---------------------------------------------------------------------------
// 2. `-n` 边界
// ---------------------------------------------------------------------------

#[test]
fn tail_n_boundaries() {
    let dir = TestDir::new("adv");
    let file = dir.file("tail.ring");

    // 已初始化但没有任何记录。
    assert_eq!(write_stdin(&file, "8K", b"").code(), Some(0));
    let out = run(&["-n", "1", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "{}", out.err());
    assert!(out.stdout.is_empty(), "空文件 -n 1 应输出空");

    assert_eq!(write_stdin(&file, "8K", b"a\nb\nc\n").code(), Some(0));

    let out = run(&["-n", "1", path_text(&file)]);
    assert_eq!(out.code(), Some(0));
    assert_eq!(out.stdout, b"c\n");

    let out = run(&["-n", "3", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "{}", out.err());
    assert_eq!(out.stdout, b"a\nb\nc\n");

    let out = run(&["-n", "100", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "{}", out.err());
    assert_eq!(out.stdout, b"a\nb\nc\n");

    let out = run(&["--lines=2", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "{}", out.err());
    assert_eq!(out.stdout, b"b\nc\n");
}

#[test]
fn tail_n_u64_max_does_not_panic() {
    let dir = TestDir::new("adv");
    let file = dir.file("maxn.ring");
    assert_eq!(write_stdin(&file, "8K", b"a\nb\n").code(), Some(0));

    let out = run(&["-n", "18446744073709551615", path_text(&file)]);
    assert!(!out.timed_out, "子进程超时");
    let stderr = out.err();
    assert_eq!(out.code(), Some(0), "-n u64::MAX 不应崩溃；stderr={stderr}");
    assert_no_panic(&stderr);
    assert_eq!(out.stdout, b"a\nb\n", "-n u64::MAX 应等价于输出全部");
}

/// `-f -n 0` 只输出启动之后的新数据，历史记录一行都不能出现。
#[test]
fn follow_n_zero_emits_only_new_data() {
    let dir = TestDir::new("adv");
    let file = dir.file("onlynew.ring");
    assert_eq!(
        write_stdin(&file, "8K", b"old 1\nold 2\nold 3\n").code(),
        Some(0)
    );

    let follower = Follower::spawn(&file, &["-n", "0", "--interval-ms", "20"]);
    thread::sleep(Duration::from_millis(500));

    let out = write_stdin(&file, "8K", b"new 1\nnew 2\n");
    assert_eq!(out.code(), Some(0), "追加失败: {}", out.err());
    assert!(
        follower.wait_out(b"new 2\n", Duration::from_secs(3)),
        "-f -n 0 没有读到新数据"
    );
    let (stdout, stderr, code) = follower.stop();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    assert!(code.is_none(), "follower 不应退出（code={code:?}）");
    assert_no_panic(&stderr);
    assert!(stderr.is_empty(), "stderr={stderr}");
    assert_eq!(stdout, b"new 1\nnew 2\n", "-f -n 0 只能输出新数据");
}

// 窗口起点正好是记录起点（total_written % capacity == 0）时，读者不应丢掉
// 唯一一条完整记录。`-n 1` 的语义是“最后 1 行”。
#[test]
fn single_full_record_window_is_visible() {
    let dir = TestDir::new("adv");
    let file = dir.file("single.ring");

    let mut first = vec![b'A'; 4095];
    first.push(b'\n');
    assert_eq!(write_stdin(&file, "8K", &first).code(), Some(0));
    let mut second = vec![b'B'; 4095];
    second.push(b'\n');
    assert_eq!(write_stdin(&file, "8K", &second).code(), Some(0));

    let slots = read_slots(&file);
    let total = slots
        .iter()
        .flatten()
        .map(|s| s.total_written)
        .max()
        .unwrap();
    assert_eq!(total, 8192, "测试前提：累计写入正好两圈");
    assert_eq!(file_len(&file), 8192);

    for n in ["1", "1000"] {
        let out = run(&["-n", n, path_text(&file)]);
        assert_eq!(out.code(), Some(0), "{}", out.err());
        assert!(
            out.stdout == second,
            "窗口里只剩一条完整 4096B 记录时，-n {n} 应输出它（实际 {} 字节）",
            out.stdout.len()
        );
    }
}

// 窗口里正好有 3 条记录、且窗口起点对齐时，`-n 3` 应输出 3 行（不是 2 行）。
#[test]
fn tail_n_larger_than_available_on_aligned_window() {
    let dir = TestDir::new("adv");
    let file = dir.file("aligned3.ring");

    let mut filler = vec![b'A'; 4095];
    filler.push(b'\n');
    assert_eq!(write_stdin(&file, "8K", &filler).code(), Some(0));

    let mut three = Vec::new();
    three.extend_from_slice(&vec![b'a'; 1000]);
    three.push(b'\n');
    three.extend_from_slice(&vec![b'b'; 1000]);
    three.push(b'\n');
    three.extend_from_slice(&vec![b'c'; 2093]);
    three.push(b'\n');
    assert_eq!(three.len(), 4096);
    assert_eq!(write_stdin(&file, "8K", &three).code(), Some(0));

    let slots = read_slots(&file);
    let total = slots
        .iter()
        .flatten()
        .map(|s| s.total_written)
        .max()
        .unwrap();
    assert_eq!(total, 8192, "测试前提：窗口起点物理偏移为 0");
    assert_eq!(file_len(&file), 8192);

    for n in ["3", "4", "1000"] {
        let out = run(&["-n", n, path_text(&file)]);
        assert_eq!(out.code(), Some(0), "{}", out.err());
        assert!(
            out.stdout == three,
            "窗口里只有 3 条完整记录时，-n {n} 应输出全部 3 条（实际 {} 字节）",
            out.stdout.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 3. 头部损坏
// ---------------------------------------------------------------------------

#[test]
fn corrupt_latest_slot_falls_back_to_previous() {
    let dir = TestDir::new("adv");
    let file = dir.file("fallback.ring");

    // 8500 条 7 字节记录，按 4096 / 4096 / 308 分批：
    // 初始化写 B(slot1)，两次 append 写 A(seq2)、B(seq3)，EOF 再写 A(seq4)。
    let all_lines: Vec<String> = (0..8500).map(|i| format!("L{i:05}\n")).collect();
    let mut groups = Vec::new();
    for range in [0..4096, 4096..8192, 8192..8500] {
        let mut group = String::new();
        for line in &all_lines[range] {
            group.push_str(line);
        }
        groups.push(group.into_bytes());
    }
    let out = write_paced(&file, "64K", &groups);
    assert_eq!(out.code(), Some(0), "写入失败: {}", out.err());
    assert_no_panic(&out.err());
    assert_eq!(file_len(&file), 65536);

    let slots = read_slots(&file);
    let (a, b) = (slots[0].unwrap(), slots[1].unwrap());
    assert_eq!((a.seq, b.seq), (4, 3), "测试前提：slot A 是最新且在用的槽");
    assert_eq!((a.total_written, b.total_written), (59500, 57344));
    assert_eq!(a.capacity, 61440);

    // 只改坏最新 slot A 的 CRC/魔法；B 仍然有效。
    corrupt_slot_crc(&file, 0);
    assert!(read_slots(&file)[0].is_none());

    let out = run(&["-n", "100000", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "读者回退失败: {}", out.err());
    let lines = records(&out.stdout);
    assert_eq!(lines.len(), 8192, "应回退到 B 的 8192 条完整记录");
    assert_eq!(lines.first().copied(), Some(b"L00000".as_slice()));
    assert_eq!(lines.last().copied(), Some(b"L08191".as_slice()));
    for line in &lines {
        assert_eq!(line.len(), 6, "回退输出不能有半行");
        assert!(
            line[0] == b'L' && line[1..].iter().all(|b| b.is_ascii_digit()),
            "回退输出出现坏行"
        );
    }

    // 写者也要回退到 B 并继续工作。
    let out = write_stdin(&file, "64K", b"RESUME\n");
    assert_eq!(out.code(), Some(0), "写者回退失败: {}", out.err());
    assert_eq!(file_len(&file), 65536, "续写后文件大小必须恒定");
    let data = dump(&file);
    assert!(data.ends_with(b"RESUME\n"), "续写数据应出现在末尾");
    assert!(data.windows(6).any(|w| w == b"L08191"), "旧数据应仍在");
}

#[test]
fn both_slots_corrupt_require_force() {
    let dir = TestDir::new("adv");
    let file = dir.file("both.ring");
    assert_eq!(write_stdin(&file, "8K", b"seed\n").code(), Some(0));
    let before = fs::read(&file).unwrap();

    corrupt_slot_crc(&file, 0);
    corrupt_slot_crc(&file, 1);
    assert!(read_slots(&file).iter().all(|slot| slot.is_none()));
    let broken = fs::read(&file).unwrap();

    let out = run(&["-n", "1", path_text(&file)]);
    assert_eq!(out.code(), Some(2), "读者应报错退出 2");
    let stderr = out.err();
    assert_no_panic(&stderr);
    assert!(
        stderr.contains("文件头") || stderr.contains("不是有效"),
        "读者报错应说明头部损坏，stderr={stderr}"
    );

    // 未加 --force 的写者不能静默覆盖。
    let out = write_stdin(&file, "8K", b"x\n");
    assert_eq!(out.code(), Some(2), "写者应报错退出 2");
    let stderr = out.err();
    assert_no_panic(&stderr);
    assert!(
        stderr.contains("--force"),
        "写者应提示 --force，stderr={stderr}"
    );
    assert_eq!(fs::read(&file).unwrap(), broken, "失败时不能改文件内容");
    assert_ne!(before, broken);

    let out = write_stdin_force(&file, "8K", b"rebuilt\n");
    assert_eq!(out.code(), Some(0), "--force 重建失败: {}", out.err());
    assert_eq!(file_len(&file), 8192);
    assert_eq!(dump(&file), b"rebuilt\n");
}

#[test]
fn inconsistent_header_invariant_is_rejected() {
    let dir = TestDir::new("adv");
    let file = dir.file("invariant.ring");
    assert_eq!(write_stdin(&file, "8K", b"AAAA\n").code(), Some(0));

    let slots = read_slots(&file);
    let (a, b) = (slots[0].unwrap(), slots[1].unwrap());
    assert!(a.seq > b.seq);
    assert_eq!((a.total_written, a.write_pos), (5, 5));
    assert_eq!(b.total_written, 0);

    // 合法 CRC，但 total_written % capacity != write_pos。
    // 如果读者接受它，dump 会读出 "AAAA\n"（外加未初始化的尾巴）。
    patch_slot(&file, 0, &build_slot(a.seq, a.epoch, a.capacity, 5, 10));
    let corrupted = read_slots(&file)[0];
    assert!(corrupted.is_none(), "测试前提：该 slot 因不变量失败而无效");

    let out = run(&["-n", "1000", path_text(&file)]);
    assert_eq!(out.code(), Some(0), "{}", out.err());
    assert!(out.stdout.is_empty(), "应回退到 B 而不是接受不变量错误的 A");

    let out = write_stdin(&file, "8K", b"ok\n");
    assert_eq!(out.code(), Some(0), "写者应回退到 B: {}", out.err());
    assert_eq!(dump(&file), b"ok\n", "写者不能从伪造的 total_written 继续");

    // 两个 slot 都 CRC 合法但违反不变量 => 整个头部无效。
    let slots = read_slots(&file);
    let (a, b) = (slots[0].unwrap(), slots[1].unwrap());
    patch_slot(
        &file,
        0,
        &build_slot(a.seq, a.epoch, a.capacity, a.write_pos, a.total_written + 1),
    );
    patch_slot(&file, 1, &build_slot(b.seq, b.epoch, b.capacity, 0, 1));
    assert!(read_slots(&file).iter().all(|slot| slot.is_none()));

    let out = run(&["-n", "1", path_text(&file)]);
    assert_eq!(out.code(), Some(2), "不变量损坏的头部应被视为无效");
    let stderr = out.err();
    assert!(
        stderr.contains("文件头") || stderr.contains("不是有效"),
        "stderr={stderr}"
    );
    let out = write_stdin(&file, "8K", b"x\n");
    assert_eq!(out.code(), Some(2));
    assert!(out.err().contains("--force"));
}

// ---------------------------------------------------------------------------
// 4. 写者被 SIGKILL 后反复重启
// ---------------------------------------------------------------------------

#[test]
fn sigkill_restart_loop_keeps_ring_consistent() {
    let dir = TestDir::new("adv");
    let file = dir.file("crash.ring");
    let size = 8192u64;

    for round in 0..20u32 {
        let mut writer = GuardChild::spawn_writer(&file, "8K");
        wait_writer_ready(&file);
        wait_until(
            || {
                file.exists()
                    && file_len(&file) == size
                    && read_slots(&file).iter().flatten().next().is_some()
            },
            Duration::from_secs(5),
            "写者初始化出有效头部",
        );

        let mut data = String::new();
        for i in 0..5000u32 {
            data.push_str(&format!("R{round:02}-{i:05}:"));
            data.extend(std::iter::repeat_n('k', 48));
            data.push('\n');
        }
        let mut stdin = writer.child_mut().stdin.take().unwrap();
        stdin.write_all(data.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(60));

        let mut child = writer.child.take().unwrap();
        let _ = child.kill();
        let status = child.wait().unwrap();
        drop(stdin);
        assert_eq!(status.signal(), Some(9), "第 {round} 轮：应被 SIGKILL 终止");
        assert_eq!(file_len(&file), size, "第 {round} 轮：文件大小必须恒定");

        let slots = read_slots(&file);
        assert!(
            slots.iter().flatten().next().is_some(),
            "第 {round} 轮：至少应有一个有效头部"
        );
        for slot in slots.iter().flatten() {
            assert_eq!(slot.capacity, 4096, "第 {round} 轮：容量错误");
            assert_eq!(
                slot.total_written % slot.capacity,
                slot.write_pos,
                "第 {round} 轮：头部不变量被破坏"
            );
        }
    }

    let out = write_stdin(&file, "8K", b"FINAL\n");
    assert_eq!(out.code(), Some(0), "崩溃后续写失败: {}", out.err());
    assert_eq!(file_len(&file), size);

    let data = dump(&file);
    let lines = records(&data);
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(
            line.is_empty() || line == b"FINAL" || crash_line_ok(line),
            "崩溃/重启后出现撕裂记录: {:?}",
            String::from_utf8_lossy(&line[..line.len().min(80)])
        );
    }
    assert_eq!(lines.last().copied(), Some(b"FINAL".as_slice()));
}

// ---------------------------------------------------------------------------
// 5. 写者互斥
// ---------------------------------------------------------------------------

#[test]
fn concurrent_writers_exclusive_lock() {
    let dir = TestDir::new("adv");
    let file = dir.file("locked.ring");

    let mut first = GuardChild::spawn_writer(&file, "8K");
    wait_writer_ready(&file);
    wait_until(
        || file.exists() && read_slots(&file).iter().flatten().next().is_some(),
        Duration::from_secs(5),
        "第一个写者初始化",
    );

    let out = write_stdin(&file, "8K", b"loser\n");
    assert_eq!(out.code(), Some(2), "第二个写者应退出码 2");
    let stderr = out.err();
    assert_no_panic(&stderr);
    assert!(stderr.contains("正被另一个写入进程使用"), "stderr={stderr}");
    assert_eq!(file_len(&file), 8192);
    assert!(dump(&file).is_empty(), "竞争失败的写者不能写入数据");

    // 第一个写者正常退出后，第三个写者应能接管。
    let _ = first.child_mut().stdin.take();
    let status = wait_status_timeout(first.child_mut(), Duration::from_secs(5))
        .expect("第一个写者应在 stdin 关闭后退出");
    assert!(status.success(), "第一个写者退出码 {:?}", status.code());

    let out = write_stdin(&file, "8K", b"third\n");
    assert_eq!(out.code(), Some(0), "第三个写者应成功: {}", out.err());
    assert_eq!(file_len(&file), 8192);
    assert_eq!(dump(&file), b"third\n");
}

/// 两个写者几乎同时启动：必须恰好一个拿到锁，另一个 exit 2。
#[test]
fn simultaneous_writers_race_one_wins() {
    let dir = TestDir::new("adv");
    let file = dir.file("race.ring");

    let mut a = GuardChild::spawn_writer(&file, "8K");
    let mut b = GuardChild::spawn_writer(&file, "8K");

    let deadline = Instant::now() + Duration::from_secs(5);
    let (loser, winner_is_a) = loop {
        let sa = a.child_mut().try_wait().unwrap();
        let sb = b.child_mut().try_wait().unwrap();
        match (sa, sb) {
            (Some(sa), None) => break (sa, false),
            (None, Some(sb)) => break (sb, true),
            (Some(_), Some(_)) => panic!("两个写者同时退出，必须恰好一个成功"),
            (None, None) => {
                assert!(Instant::now() < deadline, "两个写者都没有退出");
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(loser.code(), Some(2), "竞争失败者应 exit 2");

    let winner = if winner_is_a { &mut a } else { &mut b };
    wait_until(
        || file.exists() && read_slots(&file).iter().flatten().next().is_some(),
        Duration::from_secs(5),
        "获胜写者完成初始化",
    );
    assert_eq!(file_len(&file), 8192);
    assert!(dump(&file).is_empty());

    let _ = winner.child_mut().stdin.take();
    let status = wait_status_timeout(winner.child_mut(), Duration::from_secs(5))
        .expect("获胜写者应在 stdin 关闭后退出");
    assert!(status.success(), "获胜写者退出码 {:?}", status.code());

    let out = write_stdin(&file, "8K", b"after-race\n");
    assert_eq!(out.code(), Some(0), "胜者退出后第三个写者应成功");
    assert_eq!(dump(&file), b"after-race\n");
}

// ---------------------------------------------------------------------------
// 6. EPIPE
// ---------------------------------------------------------------------------

#[test]
fn follow_epipe_exits_zero() {
    let dir = TestDir::new("adv");
    let file = dir.file("epipe.ring");

    let mut history = String::new();
    for i in 0..100_000u32 {
        history.push_str(&format!("line {i:06}\n"));
    }
    assert!(history.len() > 1_000_000);
    let out = write_stdin(&file, "64M", history.as_bytes());
    assert_eq!(out.code(), Some(0), "准备历史数据失败: {}", out.err());

    let script = r#"set -o pipefail; timeout 10 "$1" -f "$2" | head -1"#;
    let mut cmd = Command::new("bash");
    cmd.args([
        "-c",
        script,
        "bash",
        env!("CARGO_BIN_EXE_ringlog"),
        path_text(&file),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let out = wait_capture(cmd.spawn().unwrap(), Duration::from_secs(15));
    let stderr = out.err();
    assert!(!out.timed_out, "管道命令超时（reader 没有在 EPIPE 后退出）");
    assert_eq!(
        out.code(),
        Some(0),
        "下游 head 提前关闭时 reader 应以 0 退出；stderr={stderr}"
    );
    assert_no_panic(&stderr);
    assert_eq!(out.stdout, b"line 000000\n");
}

// ---------------------------------------------------------------------------
// 7. 并发压测
// ---------------------------------------------------------------------------

#[test]
fn follow_concurrent_64m_ring_exact_output() {
    let dir = TestDir::new("adv");
    let file = dir.file("live64m.ring");
    assert_eq!(write_stdin(&file, "64M", b"").code(), Some(0));

    let follower = Follower::spawn(&file, &["-n", "0", "--interval-ms", "10"]);
    thread::sleep(Duration::from_millis(500));

    let mut input = String::new();
    for i in 0..100_000u32 {
        input.push_str(&format!("line {i:06}\n"));
    }
    let out = write_stdin(&file, "64M", input.as_bytes());
    assert_eq!(out.code(), Some(0), "并发写入失败: {}", out.err());
    assert_no_panic(&out.err());

    assert!(
        follower.wait_stdout_len(input.len(), Duration::from_secs(5)),
        "跟读没有在超时内追上，已输出 {} 字节",
        follower.stdout().len()
    );
    let (stdout, stderr, code) = follower.stop();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    assert!(
        code.is_none(),
        "follower 仍在运行时应被 kill（code={code:?}）"
    );
    assert_no_panic(&stderr);
    assert!(stderr.is_empty(), "64M 环形不应丢数据，stderr={stderr}");
    assert_eq!(stdout, input.as_bytes(), "并发跟踪输出必须逐字节一致");
}

#[test]
fn follow_concurrent_8k_wrap_only_complete_lines() {
    let dir = TestDir::new("adv");
    let file = dir.file("stress8k.ring");
    assert_eq!(write_stdin(&file, "8K", b"").code(), Some(0));

    let follower = Follower::spawn(&file, &["-n", "0", "--interval-ms", "5"]);
    thread::sleep(Duration::from_millis(500));

    let mut input = String::new();
    for i in 0..15_000u32 {
        let fill = if i % 97 == 0 {
            5000
        } else if i % 31 == 0 {
            2000
        } else {
            20
        };
        input.push_str(&format!("S{i:06}:"));
        input.extend(std::iter::repeat_n('x', fill));
        input.push('\n');
    }
    let last = b"S014999:xxxxxxxxxxxxxxxxxxxx\n";
    let out = write_stdin(&file, "8K", input.as_bytes());
    assert_eq!(out.code(), Some(0), "并发写入失败: {}", out.err());
    assert_no_panic(&out.err());
    assert_eq!(file_len(&file), 8192);

    assert!(
        follower.wait_out(last, Duration::from_secs(5)),
        "跟读没有在超时内收到最后一行，已输出 {} 字节",
        follower.stdout().len()
    );
    let (stdout, stderr, code) = follower.stop();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    assert!(
        code.is_none(),
        "follower 仍在运行时应被 kill（code={code:?}）"
    );
    assert_no_panic(&stderr);
    for line in stderr.lines() {
        assert!(
            line.starts_with("ringlog: 警告"),
            "出现非预期 stderr: {line}"
        );
    }

    let lines = records(&stdout);
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(
            stress_line_ok(line),
            "并发跟读出现撕裂记录: {:?}",
            String::from_utf8_lossy(&line[..line.len().min(80)])
        );
    }
    assert_eq!(
        lines.last().copied(),
        Some(last.strip_suffix(b"\n").unwrap())
    );
}

// ---------------------------------------------------------------------------
// 8. `--force` 重建与正在跟随的读者
// ---------------------------------------------------------------------------

#[test]
fn follow_detects_force_reset_same_size() {
    let dir = TestDir::new("adv");
    let file = dir.file("reset.ring");
    assert_eq!(
        write_stdin(&file, "8K", b"old1\nold2\nold3\n").code(),
        Some(0)
    );

    let follower = Follower::spawn(&file, &["--interval-ms", "50"]);
    assert!(
        follower.wait_out(b"old3\n", Duration::from_secs(3)),
        "follower 没有先输出历史"
    );

    let mut new_data = String::new();
    for i in 0..100u32 {
        new_data.push_str(&format!("NEW-LINE-{i:03}\n"));
    }
    // 新数据总长比旧数据大：单靠 total_written 回退无法发现重置，必须靠 epoch。
    let out = write_stdin_force(&file, "8K", new_data.as_bytes());
    assert_eq!(out.code(), Some(0), "--force 重建失败: {}", out.err());
    assert!(
        follower.wait_out(b"NEW-LINE-099\n", Duration::from_secs(3)),
        "重置后没有读到新数据"
    );
    let (stdout, stderr, code) = follower.stop();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    assert!(code.is_none(), "follower 不应退出（code={code:?}）");
    assert_no_panic(&stderr);
    assert!(
        stderr.contains("重新初始化"),
        "应告警检测到重置，stderr={stderr}"
    );
    assert!(
        stdout.ends_with(new_data.as_bytes()),
        "重置后应输出完整的新数据"
    );
}

#[test]
fn follow_handles_force_resize_as_documented() {
    let dir = TestDir::new("adv");
    let file = dir.file("resize.ring");
    assert_eq!(write_stdin(&file, "8K", b"old\n").code(), Some(0));

    let mut follower = Follower::spawn(&file, &["--interval-ms", "50"]);
    assert!(
        follower.wait_out(b"old\n", Duration::from_secs(3)),
        "follower 没有先输出历史"
    );
    let out = write_stdin_force(&file, "16K", b"new-a\nnew-b\n");
    assert_eq!(out.code(), Some(0), "--force 改大小失败: {}", out.err());

    // README：改变文件大小时数据区几何变了，读者应报错退出并提示重启。
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut status = None;
    while Instant::now() < deadline {
        if let Some(s) = follower.try_status() {
            status = Some(s);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let status = match status {
        Some(status) => status,
        None => panic!("改变文件大小后 follower 应报错退出，而不是继续跑"),
    };
    assert_eq!(status.code(), Some(2), "应按无效数据退出码 2");
    let (stdout, stderr, code) = follower.stop();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    assert_eq!(code, Some(2));
    assert_no_panic(&stderr);
    assert!(
        stderr.contains("容量已变化") && stderr.contains("重启读取进程"),
        "应给出清晰的几何变化错误，stderr={stderr}"
    );
    assert!(stdout.ends_with(b"old\n"), "报错退出前只应输出旧数据");
}

// ---------------------------------------------------------------------------
// 9. 未初始化 / 截断文件
// ---------------------------------------------------------------------------

#[test]
fn reader_rejects_uninitialized_and_truncated_files() {
    let dir = TestDir::new("adv");

    // 0 字节文件：reader 必须给出清晰错误，而不是 panic 或挂死。
    let empty = dir.file("zero.ring");
    fs::write(&empty, b"").unwrap();
    for args in [vec!["-n", "1"], vec!["--info"], vec!["-f"]] {
        let mut all = args.clone();
        all.push(path_text(&empty));
        let out = run(&all);
        assert_eq!(out.code(), Some(2), "args={args:?}");
        assert_no_panic(&out.err());
        assert!(
            out.err().contains("不是有效") || out.err().contains("文件头"),
            "args={args:?} stderr={}",
            out.err()
        );
    }

    // 0 字节文件可以被写者正常初始化（reader 仍然拒绝未初始化的空文件）。
    let out = write_stdin(&empty, "8K", b"init\n");
    assert_eq!(out.code(), Some(0), "0 字节文件应能由写者初始化");
    assert_eq!(dump(&empty), b"init\n");

    // 头部容量 12288，但文件被截断为 8192。
    let file = dir.file("truncated.ring");
    let input: String = (0..100).map(|i| format!("row {i}\n")).collect();
    assert_eq!(write_stdin(&file, "16K", input.as_bytes()).code(), Some(0));
    let handle = OpenOptions::new().write(true).open(&file).unwrap();
    handle.set_len(8192).unwrap();
    drop(handle);

    for args in [vec!["-n", "5"], vec!["--info"]] {
        let mut all = args.clone();
        all.push(path_text(&file));
        let out = run(&all);
        assert_eq!(out.code(), Some(2), "args={args:?}");
        assert_no_panic(&out.err());
        assert!(
            out.err().contains("文件大小与头区容量不一致"),
            "args={args:?} stderr={}",
            out.err()
        );
    }

    // 连头部页都不够。
    let tiny = dir.file("tiny.ring");
    fs::write(&tiny, vec![0u8; 100]).unwrap();
    let out = run(&["-n", "1", path_text(&tiny)]);
    assert_eq!(out.code(), Some(2));
    assert!(out.err().contains("不是有效") || out.err().contains("文件头"));

    // 写者不 --force 也不能把截断文件当正常文件续写。
    let out = write_stdin(&file, "8K", b"x\n");
    assert_eq!(out.code(), Some(2), "截断文件的容量与头区不一致，应拒绝");
    assert!(out.err().contains("不一致"), "stderr={}", out.err());
}

// ---------------------------------------------------------------------------
// 10. 参数组合与退出码
// ---------------------------------------------------------------------------

#[test]
fn usage_errors_exit_two() {
    let dir = TestDir::new("adv");
    let file = dir.file("args.ring");
    let p = path_text(&file);

    let cases: Vec<Vec<&str>> = vec![
        vec!["--nope"],
        vec![],
        vec!["-s", "8K"],
        vec!["-s", "4K", p],
        vec!["-s", "0", p],
        vec!["-s", "abc", p],
        vec!["-s", "8K", "-f", p],
        vec!["-s", "8K", "-n", "1", p],
        vec!["-s", "8K", "--info", p],
        vec!["-n", "0", p],
        vec!["--force", "-f", p],
        vec!["-n", "-5", p],
        vec!["-s", "8K", p, p],
    ];

    for args in cases {
        let out = run(&args);
        assert_eq!(out.code(), Some(2), "args={args:?} 应 exit 2");
        assert!(!out.timed_out, "args={args:?} 超时");
        assert_no_panic(&out.err());
        assert!(
            !out.err().trim().is_empty(),
            "args={args:?} 应有 stderr 提示"
        );
    }

    // 帮助/版式仍应成功。
    assert_eq!(run(&["--help"]).code(), Some(0));
    assert_eq!(run(&["--version"]).code(), Some(0));
}
