//! 黑盒集成测试：直接运行 ringlog 二进制，验证对外行为。

mod common;

use common::{TestDir, bin, lock_path, path_text};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn parse_size(text: &str) -> u64 {
    let (num, suffix) = text.split_at(text.len() - 1);
    let mult = match suffix {
        "K" => 1024,
        "M" => 1024 * 1024,
        _ => return text.parse().unwrap(),
    };
    num.parse::<u64>().unwrap() * mult
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().unwrap()
}

/// 以管道喂 stdin 的方式运行写入模式。
fn write_stdin(file: &Path, size: &str, input: &[u8]) -> Output {
    let mut child = bin()
        .args(["-s", size])
        .arg(file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(input);
    }
    child.wait_with_output().unwrap()
}

/// 输出现有全部有效数据。
fn dump(file: &Path) -> Vec<u8> {
    let out = run(&["-n", "100000000", path_text(file)]);
    assert!(
        out.status.success(),
        "dump 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn dump_text(file: &Path) -> String {
    String::from_utf8(dump(file)).unwrap()
}

/// 按 `\n` 切分，忽略结尾的空段。
fn lines(text: &str) -> Vec<&str> {
    text.split_terminator('\n').collect()
}

fn wait_until(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !cond() {
        assert!(Instant::now() < deadline, "等待条件超时");
        thread::sleep(Duration::from_millis(10));
    }
}

/// 启动跟随读者，后台收集 stdout/stderr。
struct Follower {
    child: Child,
    out: thread::JoinHandle<String>,
    err: thread::JoinHandle<String>,
}

impl Follower {
    fn spawn(file: &Path, extra: &[&str]) -> Self {
        let mut child = bin()
            .arg("-f")
            .args(extra)
            .arg(file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out = thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text
        });
        let err = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Self { child, out, err }
    }

    fn stop(mut self) -> (String, String) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        (self.out.join().unwrap(), self.err.join().unwrap())
    }
}

#[test]
fn write_then_dump_roundtrip() {
    let dir = TestDir::new("it");
    let file = dir.file("app.ring");
    let mut input = String::new();
    for i in 0..100 {
        input.push_str(&format!("line {i}\n"));
    }
    let out = write_stdin(&file, "64K", input.as_bytes());
    assert!(out.status.success());

    assert_eq!(dump_text(&file), input);
}

#[test]
fn file_size_never_grows() {
    let dir = TestDir::new("it");
    let file = dir.file("fixed.ring");
    let size = "8K";
    let input: String = (0..2000).map(|i| format!("row {i}\n")).collect();
    assert!(write_stdin(&file, size, input.as_bytes()).status.success());
    assert_eq!(fs::metadata(&file).unwrap().len(), parse_size(size));

    assert!(
        write_stdin(&file, size, b"after restart\n")
            .status
            .success()
    );
    assert_eq!(fs::metadata(&file).unwrap().len(), parse_size(size));
}

#[test]
fn wrap_keeps_recent_complete_lines() {
    let dir = TestDir::new("it");
    let file = dir.file("wrap.ring");
    let input: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
    assert!(write_stdin(&file, "8K", input.as_bytes()).status.success());

    let text = dump_text(&file);
    // 环绕填充的空行是数据的一部分（见 README），统计时剔除
    let kept: Vec<&str> = lines(&text).into_iter().filter(|l| !l.is_empty()).collect();
    assert!(!kept.is_empty());
    assert!(kept.len() < 1000, "容量 4 KiB 不应保留全部 1000 行");
    for line in &kept {
        // 环形接缝处不允许出现撕裂的半行
        assert!(
            line.strip_prefix("line ")
                .is_some_and(|n| n.parse::<u32>().is_ok()),
            "出现不完整行: {line:?}"
        );
    }
    assert_eq!(kept.last().copied(), Some("line 1000"));
}

#[test]
fn tail_n_returns_exact_last_lines() {
    let dir = TestDir::new("it");
    let file = dir.file("tail.ring");
    let input: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    assert!(write_stdin(&file, "64K", input.as_bytes()).status.success());

    let out = run(&["-n", "3", path_text(&file)]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "line 18\nline 19\nline 20\n"
    );
}

#[test]
fn tail_more_than_available() {
    let dir = TestDir::new("it");
    let file = dir.file("tailall.ring");
    let input = "a\nb\n";
    assert!(write_stdin(&file, "64K", input.as_bytes()).status.success());

    let out = run(&["-n", "100", path_text(&file)]);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), input);
}

#[test]
fn appends_missing_trailing_newline() {
    let dir = TestDir::new("it");
    let file = dir.file("nonl.ring");
    assert!(write_stdin(&file, "8K", b"abc").status.success());
    assert_eq!(dump(&file), b"abc\n");
}

#[test]
fn keeps_blank_lines_and_binary_bytes() {
    let dir = TestDir::new("it");
    let file = dir.file("bin.ring");
    let input: &[u8] = b"a\n\nb\n\x00\xff\xfe\n";
    assert!(write_stdin(&file, "8K", input).status.success());
    assert_eq!(dump(&file), input);
}

#[test]
fn overlong_line_is_truncated_and_warned() {
    let dir = TestDir::new("it");
    let file = dir.file("long.ring");
    let input = format!("{}\n", "A".repeat(10_000));
    let out = write_stdin(&file, "8K", input.as_bytes());
    assert!(out.status.success());
    let warning = String::from_utf8_lossy(&out.stderr);
    assert!(warning.contains("超长行"), "stderr: {warning}");

    let data = dump(&file);
    assert_eq!(data.len(), 4096, "截断到数据区容量（4095 字节 + 换行）");
    assert!(data.ends_with(b"\n"));
    assert!(data.iter().all(|&b| b == b'A' || b == b'\n'));
}

#[test]
fn empty_input_creates_empty_ring() {
    let dir = TestDir::new("it");
    let file = dir.file("empty.ring");
    assert!(write_stdin(&file, "8K", b"").status.success());
    assert!(dump(&file).is_empty());

    let info = run(&["--info", path_text(&file)]);
    assert!(info.status.success());
    let text = String::from_utf8(info.stdout).unwrap();
    assert!(text.contains("有效数据:   0 字节"), "info: {text}");
}

#[test]
fn second_writer_is_rejected() {
    let dir = TestDir::new("it");
    let file = dir.file("locked.ring");
    let mut first = bin()
        .args(["-s", "8K"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock = lock_path(&file);
    wait_until(|| lock.exists());
    // 稍等第一个写者拿到锁
    wait_until(|| std::fs::File::open(&lock).is_ok_and(|f| f.try_lock().is_err()));

    let out = run(&["-s", "8K", path_text(&file)]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("正被另一个写入进程使用"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = first.kill();
    let _ = first.wait();
}

/// 同一文件用符号链接访问也是同一个文件：第二个写者必须被拒。
#[test]
fn second_writer_via_symlink_is_rejected() {
    let dir = TestDir::new("it");
    let real = dir.file("locked.ring");
    let alias_dir = dir.file("alias");
    fs::create_dir(&alias_dir).unwrap();
    let alias = alias_dir.join("locked.ring");
    symlink(&real, &alias).unwrap();

    let mut first = bin()
        .args(["-s", "8K"])
        .arg(&real)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock = lock_path(&real);
    wait_until(|| lock.exists());
    wait_until(|| std::fs::File::open(&lock).is_ok_and(|f| f.try_lock().is_err()));

    let out = run(&["-s", "8K", path_text(&alias)]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("正被另一个写入进程使用"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = first.kill();
    let _ = first.wait();
}

/// 带 `.` 的等价路径同样归一到同一把锁。
#[test]
fn second_writer_via_dot_path_is_rejected() {
    let dir = TestDir::new("it");
    let file = dir.file("locked.ring");
    let mut first = bin()
        .args(["-s", "8K"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock = lock_path(&file);
    wait_until(|| lock.exists());
    wait_until(|| std::fs::File::open(&lock).is_ok_and(|f| f.try_lock().is_err()));

    let alias = dir.file(".").join("locked.ring");
    let out = run(&["-s", "8K", path_text(&alias)]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("正被另一个写入进程使用"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = first.kill();
    let _ = first.wait();
}

/// 通过符号链接启动写者时，锁必须落在真实文件旁，而不是符号链接所在目录。
#[test]
fn writer_lock_uses_canonical_path() {
    let dir = TestDir::new("it");
    let real = dir.file("real.ring");
    let alias_dir = dir.file("alias");
    fs::create_dir(&alias_dir).unwrap();
    let alias = alias_dir.join("real.ring");
    symlink(&real, &alias).unwrap();

    let mut writer = bin()
        .args(["-s", "8K"])
        .arg(&alias)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let real_lock = lock_path(&real);
    wait_until(|| real_lock.exists());
    wait_until(|| std::fs::File::open(&real_lock).is_ok_and(|f| f.try_lock().is_err()));
    assert!(real.exists(), "数据文件应落在符号链接指向的真实路径");
    assert!(
        !alias_dir.join("real.ring.lock").exists(),
        "锁文件不应落在符号链接所在目录"
    );

    let _ = writer.kill();
    let _ = writer.wait();
}

#[test]
fn size_over_max_rejected() {
    let dir = TestDir::new("it");
    let file = dir.file("too-big.ring");

    let out = write_stdin(&file, "2T", b"x\n");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 TiB"));
    assert!(!file.exists(), "参数校验失败不应创建文件");
}

#[test]
fn size_mismatch_is_rejected() {
    let dir = TestDir::new("it");
    let file = dir.file("mismatch.ring");
    assert!(write_stdin(&file, "16K", b"old\n").status.success());

    // 大小不一致是用法错误：ringlog 不会改大小，也没有重建开关。
    let out = write_stdin(&file, "32K", b"new\n");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("不一致")
            && String::from_utf8_lossy(&out.stderr).contains("删除该文件"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::metadata(&file).unwrap().len(),
        16 * 1024,
        "失败时不能改变文件大小"
    );

    // 原文件仍按原大小续写。
    assert!(write_stdin(&file, "16K", b"more\n").status.success());
    assert_eq!(dump_text(&file), "old\nmore\n");
}

#[test]
fn rejects_foreign_file() {
    let dir = TestDir::new("it");
    let file = dir.file("junk.bin");
    fs::write(&file, vec![0xABu8; 20 * 1024]).unwrap();

    let out = write_stdin(&file, "20K", b"x\n");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("文件头无效"));

    let out = run(&["-f", path_text(&file)]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn follow_outputs_new_data() {
    let dir = TestDir::new("it");
    let file = dir.file("live.ring");
    assert!(
        write_stdin(&file, "64K", b"old 1\nold 2\n")
            .status
            .success()
    );

    let follower = Follower::spawn(&file, &["-n", "0"]);
    thread::sleep(Duration::from_millis(300));
    assert!(
        write_stdin(&file, "64K", b"live 1\nlive 2\n")
            .status
            .success()
    );
    thread::sleep(Duration::from_millis(400));

    let (stdout, stderr) = follower.stop();
    assert_eq!(stdout, "live 1\nlive 2\n");
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

#[test]
fn follow_starts_with_last_lines() {
    let dir = TestDir::new("it");
    let file = dir.file("hist.ring");
    assert!(
        write_stdin(&file, "64K", b"a\nb\nc\nd\ne\n")
            .status
            .success()
    );

    let follower = Follower::spawn(&file, &["-n", "2"]);
    thread::sleep(Duration::from_millis(300));
    assert!(write_stdin(&file, "64K", b"f\n").status.success());
    thread::sleep(Duration::from_millis(400));

    let (stdout, stderr) = follower.stop();
    assert_eq!(stdout, "d\ne\nf\n");
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

#[test]
fn slow_reader_warns_and_skips_old_data() {
    let dir = TestDir::new("it");
    let file = dir.file("slow.ring");
    assert!(write_stdin(&file, "8K", b"").status.success());

    let follower = Follower::spawn(&file, &["-n", "0", "--interval-ms", "20"]);
    thread::sleep(Duration::from_millis(200));
    let payload: String = (0..2000).map(|i| format!("row {i}\n")).collect();
    assert!(
        write_stdin(&file, "8K", payload.as_bytes())
            .status
            .success()
    );
    thread::sleep(Duration::from_millis(400));

    let (stdout, stderr) = follower.stop();
    assert!(
        stderr.contains("已被覆盖"),
        "应当告警丢数据，stderr: {stderr}"
    );
    let got: Vec<&str> = lines(&stdout)
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect();
    assert!(!got.is_empty());
    assert_eq!(got.last().copied(), Some("row 1999"));
    for line in &got {
        assert!(
            line.strip_prefix("row ")
                .is_some_and(|n| n.parse::<u32>().is_ok()),
            "出现不完整行: {line:?}"
        );
    }
}

#[test]
fn crash_mid_stream_then_restart() {
    let dir = TestDir::new("it");
    let file = dir.file("crash.ring");
    let mut child = bin()
        .args(["-s", "64K"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // 写入超过一个落批阈值（256 KiB），保证头部已经更新过
    let line = format!("{}\n", "k".repeat(1023));
    let mut chunk = String::new();
    for _ in 0..300 {
        chunk.push_str(&line);
    }
    stdin.write_all(chunk.as_bytes()).unwrap();
    drop(stdin);
    thread::sleep(Duration::from_millis(100));
    let _ = child.kill();
    let _ = child.wait();

    // 崩溃后重启：文件应仍可写、可读
    assert!(write_stdin(&file, "64K", b"after crash\n").status.success());
    let text = dump_text(&file);
    let kept = lines(&text);
    assert_eq!(kept.last().copied(), Some("after crash"));
    assert!(
        kept.iter()
            .any(|l| l.len() == 1023 && l.bytes().all(|b| b == b'k'))
    );
}

#[test]
fn help_and_version() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("用法:"));

    let version = run(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("ringlog "));
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(run(&["--nope"]).status.code(), Some(2));
    assert_eq!(run(&[]).status.code(), Some(2));
    let out = run(&["-n", "0", "/tmp/whatever.ring"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn reader_rejects_uninitialized_file() {
    let dir = TestDir::new("it");
    let file = dir.file("zero.ring");
    fs::write(&file, b"").unwrap();
    let out = run(&["-f", path_text(&file)]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("不是有效的 ringlog 文件"));
}

#[test]
fn broken_pipe_makes_follower_exit_zero() {
    let dir = TestDir::new("it");
    let file = dir.file("pipe.ring");
    assert!(write_stdin(&file, "8K", b"").status.success());

    let mut child = bin()
        .args(["-f", "-n", "0", "--interval-ms", "20"])
        .arg(&file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    thread::sleep(Duration::from_millis(200));
    drop(stdout); // 下游关闭
    thread::sleep(Duration::from_millis(100));
    assert!(write_stdin(&file, "8K", b"x\n").status.success());

    let status = child.wait().unwrap();
    assert!(status.success(), "EPIPE 应安静退出，实际: {status:?}");
}

#[test]
fn concurrent_reader_never_outputs_torn_lines() {
    let dir = TestDir::new("it");
    let file = dir.file("torn.ring");
    assert!(write_stdin(&file, "8K", b"").status.success());

    let follower = Follower::spawn(&file, &["-n", "0", "--interval-ms", "10"]);
    thread::sleep(Duration::from_millis(200));
    let payload: String = (0..3000).map(|i| format!("row {i}\n")).collect();
    assert!(
        write_stdin(&file, "8K", payload.as_bytes())
            .status
            .success()
    );
    thread::sleep(Duration::from_millis(300));

    let (stdout, _stderr) = follower.stop();
    for line in stdout.split_terminator('\n') {
        if line.is_empty() {
            continue; // 环绕填充的空行
        }
        assert!(
            line.strip_prefix("row ")
                .is_some_and(|n| n.parse::<u32>().is_ok()),
            "出现撕裂行: {line:?}"
        );
    }
}

#[test]
fn newest_header_slot_corruption_falls_back() {
    let dir = TestDir::new("it");
    let file = dir.file("slot.ring");
    assert!(write_stdin(&file, "8K", b"a\nb\nc\n").status.success());

    // 初始化 seq=1 落在 slot B（偏移 64）；追加一批后 seq=2 落在 slot A（偏移 0，最新）。
    // 改坏 slot A 里的 seq 字段（偏移 16..24），其 CRC 随之失效。
    let mut data = fs::read(&file).unwrap();
    assert_eq!(data.len(), 8192);
    data[20] ^= 0xff;
    fs::write(&file, &data).unwrap();

    // 回退到旧头部：内容为空但不报错
    let out = run(&["-n", "10", path_text(&file)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());

    // 写者可以在回退状态上继续写
    assert!(write_stdin(&file, "8K", b"z\n").status.success());
    assert_eq!(dump_text(&file), "z\n");
}

#[test]
fn closed_stderr_does_not_crash_writer() {
    let dir = TestDir::new("it");
    let file = dir.file("nostderr.ring");
    let mut child = bin()
        .args(["-s", "8K"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", "L".repeat(10_000)).as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(dump(&file).len(), 4096);
}

#[test]
fn writer_flushes_low_rate_input_promptly() {
    let dir = TestDir::new("it");
    let file = dir.file("prompt.ring");
    let mut writer = bin()
        .args(["-s", "64K"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = writer.stdin.take().unwrap();
    wait_until(|| fs::metadata(&file).is_ok_and(|m| m.len() > 0));

    let follower = Follower::spawn(&file, &["-n", "0", "--interval-ms", "20"]);
    thread::sleep(Duration::from_millis(150));
    // 只写一行且不关闭 stdin：写者应在一个落盘周期内让跟随端看到
    stdin.write_all(b"slow line\n").unwrap();
    thread::sleep(Duration::from_millis(500));

    let (stdout, stderr) = follower.stop();
    assert_eq!(stdout, "slow line\n");
    assert!(stderr.is_empty(), "stderr: {stderr}");

    drop(stdin);
    let _ = writer.wait();
}

#[test]
fn writer_flushes_steady_low_rate_input() {
    let dir = TestDir::new("it");
    let file = dir.file("steady.ring");
    let mut writer = bin()
        .args(["-s", "64K"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = writer.stdin.take().unwrap();
    wait_until(|| fs::metadata(&file).is_ok_and(|m| m.len() > 0));

    let follower = Follower::spawn(&file, &["-n", "0", "--interval-ms", "20"]);
    thread::sleep(Duration::from_millis(150));
    // 每 30ms 一行：输入永远不空闲，但落盘仍应在 100ms 周期内发生
    for i in 0..10 {
        stdin.write_all(format!("steady {i}\n").as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(30));
    }
    thread::sleep(Duration::from_millis(250));

    let (stdout, _stderr) = follower.stop();
    let got = lines(&stdout);
    assert_eq!(got.len(), 10, "实际输出: {stdout:?}");
    assert_eq!(got[0], "steady 0");
    assert_eq!(got[9], "steady 9");

    drop(stdin);
    let _ = writer.wait();
}
