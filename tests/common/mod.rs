//! 集成测试共用的临时目录与二进制入口。
//!
//! 两个测试二进制各自只用其中一部分，因此允许存在未使用的项。

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 每个测试独立的临时目录，Drop 时清理。
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(prefix: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("ringlog-{prefix}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 被测二进制（由 cargo 注入的路径）。
pub fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ringlog"))
}

pub fn path_text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// 侧车锁文件路径：按目标文件的规范路径派生（与写者实现一致），
/// 否则通过符号链接或带 `..`/`.` 的路径找不到写者实际持有的那把锁。
pub fn lock_path(file: &Path) -> PathBuf {
    ringlog::ring::lock_path(file).unwrap()
}
