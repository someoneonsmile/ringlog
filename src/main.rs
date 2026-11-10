//! ringlog 命令行入口：解析参数、分派模式、决定退出码。

mod app;

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use ringlog::cli::{self, Cli, Mode};
use ringlog::follow;
use ringlog::ring::Reader;

use app::{info, report, writer};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // clap 负责 --help / --version（退出码 0）和参数错误（退出码 2）。
    let cli = match cli::parse(args) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(err.exit_code() as u8);
        }
    };

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        // 下游（head、grep -m 等）提前关闭管道是正常用法，安静退出。
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            report::warn(format_args!("{e}"));
            match e.kind() {
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => ExitCode::from(2),
                _ => ExitCode::from(1),
            }
        }
    }
}

fn run(cli: &Cli) -> io::Result<()> {
    match cli.mode {
        Mode::Write { size, sync, force } => writer::run(&cli.path, size, force, sync),
        Mode::Read {
            follow: follow_mode,
            start,
            interval,
        } => {
            let reader = Reader::open(&cli.path)?;
            follow::run(&reader, follow_mode, start, interval)
        }
        Mode::Info => info::run(&cli.path),
    }
}
