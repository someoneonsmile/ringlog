//! 文件读写层。
//!
//! - 写者（[`Ring`]）：持有侧车锁文件保证进程级互斥；每批记录在文件级互斥锁内
//!   写入数据、再更新头部，保证读者永远看不到写了一半的记录。
//! - 读者（[`Reader`]）：在文件级共享锁内读取「头部 + 数据」，拿到一致快照。
//!
//! 数据区是真正的环形：空间不足时用换行填充到文件尾，再从 0 继续写，
//! 因此任何时刻数据区里都是一串以 `\n` 结尾的完整记录。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::error::{invalid, usage};
use crate::format::{
    DATA_OFFSET, HEADER_PAGE, Header, MIN_DATA_CAPACITY, MIN_TOTAL_SIZE, SLOT_BYTES, SLOT_OFFSETS,
    data_capacity, file_size,
};
use crate::input::Batch;

/// 环绕填充每次写入的块大小。
const PAD_CHUNK: usize = 64 * 1024;
static PAD_BYTES: [u8; PAD_CHUNK] = [b'\n'; PAD_CHUNK];

/// 写者互斥锁文件的路径：`<FILE>.lock`。
pub fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// 读取头区，返回两个 slot 中序号更大的有效头部。
pub fn read_header(file: &File) -> io::Result<Option<Header>> {
    if file.metadata()?.len() < HEADER_PAGE {
        return Ok(None);
    }
    let mut page = [0u8; HEADER_PAGE as usize];
    file.read_exact_at(&mut page, 0)?;
    let mut best: Option<Header> = None;
    for off in SLOT_OFFSETS {
        let off = off as usize;
        let mut buf = [0u8; SLOT_BYTES];
        buf.copy_from_slice(&page[off..off + SLOT_BYTES]);
        if let Some(header) = Header::from_slot(&buf)
            && best.is_none_or(|b| header.seq > b.seq)
        {
            best = Some(header);
        }
    }
    Ok(best)
}

fn write_slot(file: &File, header: &Header) -> io::Result<()> {
    let off = SLOT_OFFSETS[(header.seq & 1) as usize];
    file.write_all_at(&header.to_slot(), off)
}

/// 写者。
pub struct Ring {
    file: File,
    capacity: u64,
    write_pos: u64,
    total_written: u64,
    seq: u64,
    /// 文件重置纪元；`--force` 重建时 +1，读者据此识别“文件被重置”。
    epoch: u64,
    sync: bool,
    /// 侧车锁文件的句柄；存活期间独占，进程退出/崩溃时由内核释放。
    _writer_lock: File,
}

impl Ring {
    /// 打开或创建环形文件。
    ///
    /// `total_size` 是文件总大小（含 4 KiB 头区）；`force` 会丢弃已有数据并重建。
    pub fn open(path: &Path, total_size: u64, force: bool, sync: bool) -> io::Result<Self> {
        let lock_file = lock_path(path);
        let writer_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_file)
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("无法创建锁文件 {}: {e}", lock_file.display()),
                )
            })?;
        match writer_lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(usage(format!(
                    "{} 正被另一个写入进程使用（锁文件：{}）",
                    path.display(),
                    lock_file.display()
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| io::Error::new(e.kind(), format!("无法打开 {}: {e}", path.display())))?;
        let len = file.metadata()?.len();
        let existing = read_header(&file)?;

        let (capacity, write_pos, total_written, seq, epoch) = if force || len == 0 {
            let capacity = data_capacity(total_size).ok_or_else(|| {
                usage(format!(
                    "文件总大小至少需要 {MIN_TOTAL_SIZE} 字节（4 KiB 头区 + {MIN_DATA_CAPACITY} 字节数据区），当前 -s 为 {total_size} 字节"
                ))
            })?;
            file.set_len(total_size)?;
            let seq = existing.map_or(1, |h| h.seq.wrapping_add(1));
            let epoch = existing.map_or(1, |h| h.epoch.wrapping_add(1).max(1));
            let header = Header::new(seq, epoch, capacity);
            write_slot(&file, &header)?;
            if sync {
                file.sync_data()?;
            }
            (capacity, 0, 0, seq, epoch)
        } else {
            let capacity = data_capacity(len).ok_or_else(|| {
                invalid(format!(
                    "{} 只有 {} 字节，不是有效的 ringlog 文件",
                    path.display(),
                    len
                ))
            })?;
            if total_size != len {
                return Err(usage(format!(
                    "{} 现有大小 {} 字节，与 -s {} 不一致；确认要重建请加 --force",
                    path.display(),
                    len,
                    total_size
                )));
            }
            let header = existing.ok_or_else(|| {
                invalid(format!(
                    "{} 的文件头无效（不是 ringlog 文件或已损坏）；确认覆盖请加 --force",
                    path.display()
                ))
            })?;
            if header.capacity != capacity {
                return Err(invalid(format!(
                    "{} 头区记录的容量 {} 与实际数据区 {} 不一致；确认重建请加 --force",
                    path.display(),
                    header.capacity,
                    capacity
                )));
            }
            (
                capacity,
                header.write_pos,
                header.total_written,
                header.seq,
                header.epoch,
            )
        };

        Ok(Self {
            file,
            capacity,
            write_pos,
            total_written,
            seq,
            epoch,
            sync,
            _writer_lock: writer_lock,
        })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// 追加一批记录。每条记录必须以 `\n` 结尾，且长度不超过 `capacity`。
    pub fn append(&mut self, batch: Batch) -> io::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        self.file.lock()?;
        let result = self.append_locked(&batch);
        let unlock_result = self.file.unlock();
        result?;
        unlock_result
    }

    fn append_locked(&mut self, batch: &Batch) -> io::Result<()> {
        let payload = &batch.payload;
        let cap = self.capacity;
        let mut pos = self.write_pos;
        let mut total = self.total_written;

        enum Op {
            Data {
                file_off: u64,
                start: usize,
                len: usize,
            },
            Pad {
                file_off: u64,
                len: u64,
            },
        }

        let mut ops: Vec<Op> = Vec::new();
        for record in &batch.records {
            let (start, len) = (record.start, record.len);
            let len_u = len as u64;
            debug_assert!(len_u <= cap, "单条记录不能超过数据区容量");
            if cap - pos < len_u {
                // 剩余空间放不下这条记录：填满换行，保证记录不被环形边界拆断。
                let pad = cap - pos;
                ops.push(Op::Pad {
                    file_off: DATA_OFFSET + pos,
                    len: pad,
                });
                total += pad;
                pos = 0;
            }
            let file_off = DATA_OFFSET + pos;
            let merged = match ops.last_mut() {
                Some(Op::Data {
                    file_off: last_off,
                    start: last_start,
                    len: last_len,
                }) if *last_off + *last_len as u64 == file_off
                    && *last_start + *last_len == start =>
                {
                    *last_len += len;
                    true
                }
                _ => false,
            };
            if !merged {
                ops.push(Op::Data {
                    file_off,
                    start,
                    len,
                });
            }
            pos = (pos + len_u) % cap;
            total += len_u;
        }

        for op in &ops {
            match op {
                Op::Pad { file_off, len } => self.write_pad(*file_off, *len)?,
                Op::Data {
                    file_off,
                    start,
                    len,
                } => self
                    .file
                    .write_all_at(&payload[*start..*start + *len], *file_off)?,
            }
        }
        if self.sync {
            // 先把数据落盘，再发布头部，避免头部指向未落盘的数据。
            self.file.sync_data()?;
        }

        self.seq += 1;
        let header = Header {
            seq: self.seq,
            epoch: self.epoch,
            capacity: cap,
            write_pos: pos,
            total_written: total,
        };
        write_slot(&self.file, &header)?;
        if self.sync {
            self.file.sync_data()?;
        }

        self.write_pos = pos;
        self.total_written = total;
        Ok(())
    }

    fn write_pad(&self, mut file_off: u64, mut len: u64) -> io::Result<()> {
        while len > 0 {
            let n = len.min(PAD_CHUNK as u64) as usize;
            self.file.write_all_at(&PAD_BYTES[..n], file_off)?;
            file_off += n as u64;
            len -= n as u64;
        }
        Ok(())
    }
}

/// 读者：只读打开，在共享锁内获取一致快照。
pub struct Reader {
    file: File,
    capacity: u64,
}

impl Reader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|e| io::Error::new(e.kind(), format!("无法打开 {}: {e}", path.display())))?;
        let len = file.metadata()?.len();
        let header = read_header(&file)?.ok_or_else(|| {
            invalid(format!(
                "{} 不是有效的 ringlog 文件（文件头缺失或损坏）",
                path.display()
            ))
        })?;
        if file_size(header.capacity) != len {
            return Err(invalid(format!(
                "{} 的文件大小与头区容量不一致（文件可能被截断或重建）",
                path.display()
            )));
        }
        Ok(Self {
            file,
            capacity: header.capacity,
        })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn file_len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// 在共享锁内执行 `f`，保证「读到的头部」与「读到的数据」来自同一一致状态。
    pub fn with_snapshot<T>(
        &self,
        f: impl FnOnce(&Reader, &Header) -> io::Result<T>,
    ) -> io::Result<T> {
        self.file.lock_shared()?;
        let result = (|| {
            let header = read_header(&self.file)
                .map_err(|e| invalid(format!("读取文件头失败（文件可能正在被重建）: {e}")))?
                .ok_or_else(|| invalid("读取过程中文件头失效（写入进程可能正在初始化文件）"))?;
            if header.capacity != self.capacity {
                return Err(invalid("文件容量已变化（文件可能被重建），请重启读取进程"));
            }
            f(self, &header)
        })();
        let _ = self.file.unlock();
        result
    }

    /// 从逻辑偏移 `abs` 起读满 `buf`；自动处理环形回绕。
    pub fn read_exact_abs(&self, abs: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut abs = abs;
        let mut rest = buf;
        while !rest.is_empty() {
            let off = abs % self.capacity;
            let n = (self.capacity - off).min(rest.len() as u64) as usize;
            let (head, tail) = rest.split_at_mut(n);
            self.file.read_exact_at(head, DATA_OFFSET + off)?;
            rest = tail;
            abs += n as u64;
        }
        Ok(())
    }
}
