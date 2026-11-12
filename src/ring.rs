//! 文件读写层。
//!
//! - 写者（[`Ring`]）：持有侧车锁文件（按目标文件的规范路径派生）保证进程级互斥；
//!   每批记录在文件级互斥锁内写入数据、再更新头部，保证读者永远看不到写了一半的记录。
//! - 读者（[`Reader`]）：在文件级共享锁内读取「头部 + 数据」，拿到一致快照。
//!
//! 数据区是真正的环形：空间不足时用换行填充到文件尾，再从 0 继续写，
//! 因此任何时刻数据区里都是一串以 `\n` 结尾的完整记录。

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::error::{invalid, usage};
use crate::format::{
    DATA_OFFSET, HEADER_PAGE, Header, HeaderError, MIN_DATA_CAPACITY, MIN_TOTAL_SIZE, SLOT_BYTES,
    SLOT_OFFSETS, data_capacity, file_size,
};
use memchr::memchr_iter;

/// 环绕填充每次写入的块大小。
const PAD_CHUNK: usize = 64 * 1024;
static PAD_BYTES: [u8; PAD_CHUNK] = [b'\n'; PAD_CHUNK];

/// 写者互斥锁文件的路径：目标文件规范路径的 `<FILE>.lock`。
///
/// 锁按「文件」而不是「路径字符串」取：相对路径、`..`、符号链接都归一到同一把锁，
/// 否则同一个文件的两种写法会各自建一个锁文件，单写者保证被绕过。目标文件尚不存在
/// （首次创建）时退化为「规范化父目录 + 文件名」。
pub fn lock_path(path: &Path) -> io::Result<PathBuf> {
    let mut name = canonical_target(path)?.into_os_string();
    name.push(".lock");
    Ok(PathBuf::from(name))
}

/// 解析目标文件的稳定绝对路径；文件尚不存在时解析其父目录。
fn canonical_target(path: &Path) -> io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let name = path
                .file_name()
                .ok_or_else(|| usage(format!("{} 不是有效的文件路径", path.display())))?;
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            Ok(parent.canonicalize()?.join(name))
        }
        Err(error) => Err(error),
    }
}

/// 头区读不出来时的原因，用来给用户可操作的提示。
#[derive(Debug, Clone, Copy)]
enum HeaderFailure {
    /// 文件长度连 4 KiB 头区都不够（写者会初始化空文件，读者会拒绝）。
    Truncated { len: u64 },
    /// 两个 slot 都解析失败；取其中信息量最大的原因。
    Invalid(HeaderError),
}

impl fmt::Display for HeaderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { len } => {
                write!(f, "文件只有 {len} 字节，不足 {HEADER_PAGE} 字节头区")
            }
            Self::Invalid(error) => write!(f, "{error}"),
        }
    }
}

/// 读取头区，返回两个 slot 中序号更大的有效头部。
///
/// 外层 `Err` 是 I/O 失败；内层 `Err` 表示「文件读出来了，但不能当 ringlog 用」，
/// 附带具体原因（不是 ringlog / 版本不支持 / 头部损坏），供上层给出准确提示。
fn read_header(file: &File) -> io::Result<Result<Header, HeaderFailure>> {
    let len = file.metadata()?.len();
    if len < HEADER_PAGE {
        return Ok(Err(HeaderFailure::Truncated { len }));
    }
    let mut page = [0u8; HEADER_PAGE as usize];
    file.read_exact_at(&mut page, 0)?;
    let mut best: Option<Header> = None;
    let mut failure: Option<HeaderError> = None;
    for off in SLOT_OFFSETS {
        let off = off as usize;
        let mut buf = [0u8; SLOT_BYTES];
        buf.copy_from_slice(&page[off..off + SLOT_BYTES]);
        match Header::from_slot(&buf) {
            Ok(header) if best.is_none_or(|b| header.seq > b.seq) => best = Some(header),
            Ok(_) => {}
            Err(error) => {
                failure = Some(match failure {
                    Some(previous) => preferred_error(previous, error),
                    None => error,
                });
            }
        }
    }
    Ok(match best {
        Some(header) => Ok(header),
        // 两个 slot 都不是有效头部，failure 必然已有值；兜底用 Corrupt。
        None => Err(HeaderFailure::Invalid(
            failure.unwrap_or(HeaderError::Corrupt),
        )),
    })
}

/// 两个 slot 都无效时保留信息量更大的原因：不是 ringlog < 已损坏 < 版本不支持。
///
/// 混合状态（一个 slot 像 ringlog、另一个是垃圾）应报「损坏」；而文件明显是新版
/// ringlog 写的时候应报「版本不支持」——后者才是用户真正需要知道的事实。
fn preferred_error(current: HeaderError, next: HeaderError) -> HeaderError {
    fn rank(error: HeaderError) -> u8 {
        match error {
            HeaderError::NotRinglog => 0,
            HeaderError::Corrupt => 1,
            HeaderError::UnsupportedVersion { .. } => 2,
        }
    }
    if rank(next) > rank(current) {
        next
    } else {
        current
    }
}

/// 调用方身份：决定错误提示的口吻与处置建议。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Writer,
    Reader,
}

/// 头区不可用时的用户提示。
///
/// 写者与读者的开头语不同：写者强调「不会覆盖已有文件」，读者强调「这个文件读不了」。
/// 版本不支持单独成一类——文件本身没坏，只是本程序太旧，不能报成「文件损坏」。
fn header_error(path: &Path, failure: HeaderFailure, role: Role) -> io::Error {
    let writer = role == Role::Writer;
    let unsupported = matches!(
        failure,
        HeaderFailure::Invalid(HeaderError::UnsupportedVersion { .. })
    );
    let opening = match (writer, unsupported) {
        (_, true) => "的格式版本不受支持",
        (true, false) => "的文件头无效",
        (false, false) => "不是有效的 ringlog 文件",
    };
    let remedy = if unsupported {
        if writer {
            "；请改用支持该格式版本的 ringlog（本程序不会覆盖已有文件）"
        } else {
            "；请改用支持该格式版本的 ringlog"
        }
    } else if writer {
        "；ringlog 不会覆盖已有文件，如需重建请先停止写入进程并删除该文件"
    } else if matches!(failure, HeaderFailure::Truncated { .. }) {
        "；文件可能尚未初始化"
    } else {
        ""
    };
    invalid(format!("{} {opening}：{failure}{remedy}", path.display()))
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
    sync: bool,
    /// 侧车锁文件的句柄；存活期间独占，进程退出/崩溃时由内核释放。
    _writer_lock: File,
}

/// 续写已有文件时需要恢复的写入状态。
struct State {
    capacity: u64,
    write_pos: u64,
    total_written: u64,
    seq: u64,
}

/// 取得写者独占锁；同一文件已有写者时返回用法错误。
fn acquire_writer_lock(path: &Path) -> io::Result<File> {
    let lock_file_path = lock_path(path)
        .map_err(|e| io::Error::new(e.kind(), format!("无法确定锁文件路径: {e}")))?;
    let writer_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_file_path)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("无法创建锁文件 {}: {e}", lock_file_path.display()),
            )
        })?;
    match writer_lock.try_lock() {
        Ok(()) => Ok(writer_lock),
        Err(TryLockError::WouldBlock) => Err(usage(format!(
            "{} 正被另一个写入进程使用（锁文件：{}）",
            path.display(),
            lock_file_path.display()
        ))),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn open_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("无法打开 {}: {e}", path.display())))
}

/// 初始化空文件，返回从零开始的写入状态。
fn initialize(file: &File, total_size: u64, sync: bool) -> io::Result<State> {
    let capacity = data_capacity(total_size).ok_or_else(|| {
        usage(format!(
            "文件总大小至少需要 {MIN_TOTAL_SIZE} 字节（4 KiB 头区 + {MIN_DATA_CAPACITY} 字节数据区），当前 -s 为 {total_size} 字节"
        ))
    })?;
    file.set_len(total_size)?;
    let seq = 1;
    write_slot(file, &Header::new(seq, capacity))?;
    if sync {
        file.sync_data()?;
    }
    Ok(State {
        capacity,
        write_pos: 0,
        total_written: 0,
        seq,
    })
}

/// 校验已有文件并恢复续写状态。
fn resume(file: &File, path: &Path, total_size: u64, header: Header) -> io::Result<State> {
    let len = file.metadata()?.len();
    let capacity = data_capacity(len).ok_or_else(|| {
        invalid(format!(
            "{} 只有 {} 字节，不是有效的 ringlog 文件",
            path.display(),
            len
        ))
    })?;
    if total_size != len {
        return Err(usage(format!(
            "{} 现有大小 {} 字节，与 -s {} 不一致；ringlog 不允许改变已有文件的大小。如需重建，请先停止写入进程并删除该文件",
            path.display(),
            len,
            total_size
        )));
    }
    if header.capacity != capacity {
        return Err(invalid(format!(
            "{} 头区记录的容量 {} 与实际数据区 {} 不一致（头部损坏或文件被外部改写）。如需重建，请先停止写入进程并删除该文件",
            path.display(),
            header.capacity,
            capacity
        )));
    }
    Ok(State {
        capacity,
        write_pos: header.write_pos,
        total_written: header.total_written,
        seq: header.seq,
    })
}

impl Ring {
    /// 打开或创建环形文件。
    ///
    /// `total_size` 是文件总大小（含 4 KiB 头区）。文件不存在或为空时按 `total_size`
    /// 初始化；已有文件必须是同尺寸的有效 ringlog 文件，ringlog 不会重建它或改变它的大小。
    pub fn open(path: &Path, total_size: u64, sync: bool) -> io::Result<Self> {
        // 先打开（必要时创建）数据文件，再取锁：这样符号链接即使指向尚不存在的目标，
        // 也会在创建后解析到真实路径，锁才能落在同一个文件上。
        let file = open_file(path)?;
        let writer_lock = acquire_writer_lock(path)?;
        let state = if file.metadata()?.len() == 0 {
            initialize(&file, total_size, sync)?
        } else {
            let header =
                read_header(&file)?.map_err(|failure| header_error(path, failure, Role::Writer))?;
            resume(&file, path, total_size, header)?
        };
        Ok(Self {
            file,
            capacity: state.capacity,
            write_pos: state.write_pos,
            total_written: state.total_written,
            seq: state.seq,
            sync,
            _writer_lock: writer_lock,
        })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// 追加一批记录。`payload` 必须是一串以 `\n` 结尾、长度不超过 `capacity` 的记录。
    pub fn append(&mut self, payload: Vec<u8>) -> io::Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        self.file.lock()?;
        let result = self.append_locked(&payload);
        let unlock_result = self.file.unlock();
        result?;
        unlock_result
    }

    fn append_locked(&mut self, payload: &[u8]) -> io::Result<()> {
        let cap = self.capacity;

        let (pos, total) = if payload.len() as u64 <= cap - self.write_pos {
            // 快路径：整批都落在当前环形窗口内，一次写完，不需要按记录切分。
            self.file
                .write_all_at(payload, DATA_OFFSET + self.write_pos)?;
            let written = payload.len() as u64;
            (
                (self.write_pos + written) % cap,
                self.total_written + written,
            )
        } else {
            self.place_across_wrap(payload)?
        };

        if self.sync {
            // 先把数据落盘，再发布头部，避免头部指向未落盘的数据。
            self.file.sync_data()?;
        }

        self.seq += 1;
        let header = Header {
            seq: self.seq,
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

    /// 慢路径：整批会跨越环形边界，按记录边界摆放，必要时填满换行再绕回。
    fn place_across_wrap(&self, payload: &[u8]) -> io::Result<(u64, u64)> {
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
        let mut start = 0;
        for i in memchr_iter(b'\n', payload) {
            let len = i + 1 - start;
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
            start = i + 1;
        }
        debug_assert_eq!(start, payload.len(), "payload 必须以换行结尾");

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
        Ok((pos, total))
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
        let header =
            read_header(&file)?.map_err(|failure| header_error(path, failure, Role::Reader))?;
        if file_size(header.capacity) != len {
            return Err(invalid(format!(
                "{} 的文件大小与头区容量不一致（文件可能被截断或外部改写）",
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
                .map_err(|e| invalid(format!("读取文件头失败（文件可能被外部改写）: {e}")))?
                .map_err(|failure| {
                    invalid(format!(
                        "读取过程中文件头失效（{failure}），文件可能被截断或正在初始化"
                    ))
                })?;
            if header.capacity != self.capacity {
                return Err(invalid(
                    "文件容量已变化（文件可能被外部改写），请重启读取进程",
                ));
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
