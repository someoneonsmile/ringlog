//! ringlog 文件格式（v1）。
//!
//! 布局：
//!
//! ```text
//! 偏移 0     4 KiB 头区
//!   +0       slot A（64 字节）
//!   +64      slot B（64 字节）
//! 偏移 4096  数据区（环形，容量 = 文件大小 - 4096）
//! ```
//!
//! 头区两个 slot 交替写入，各带 crc32c 校验；读取时取序号更大的有效 slot。
//! 某次写头被撕裂（断电、被杀）时，可以回退到上一个一致状态。
//!
//! slot 布局（小端）：
//!
//! ```text
//! 0..8    magic         b"RINGLOG\0"
//! 8..12   version       = 1
//! 12..16  slot_size     = 64
//! 16..24  seq           每次更新 +1，决定 slot 新旧与奇偶
//! 24..32  capacity      数据区容量（字节）
//! 32..40  write_pos     下一次写入在数据区内的偏移
//! 40..48  total_written 累计写入数据区（含环绕填充）的字节数
//! 48..60  reserved      保留，0
//! 60..64  crc32c(buf[0..60])
//! ```
//!
//! 不变量：`total_written % capacity == write_pos`。
//!
//! # 版本策略
//!
//! - `0..16`（magic、version、slot_size）在任何版本里位置固定，且解析时**先校验这三项、
//!   再校验 CRC**。这样旧程序读到新版程序写的文件时会报 [`HeaderError::UnsupportedVersion`]，
//!   而不是把 CRC 范围不同误报成「文件损坏」。
//! - 只有不兼容的布局变化才递增 [`VERSION`]；向后兼容地追加字段写进 `reserved`，不递增
//!   （旧程序忽略这些字节，CRC 仍然吻合）。
//! - 读取端要求版本**严格相等**，不做「尽力解析」：读不懂就不读。
//!
//! magic 里不带版本号：它只回答「这是不是 ringlog 文件」，版本只由 `version` 字段回答。

use std::fmt;

use crc32c::crc32c;

pub(super) const HEADER_PAGE: u64 = 4096;
pub(super) const DATA_OFFSET: u64 = HEADER_PAGE;
pub(super) const SLOT_SIZE: u64 = 64;
pub(super) const SLOT_BYTES: usize = SLOT_SIZE as usize;
pub(super) const SLOT_OFFSETS: [u64; 2] = [0, SLOT_SIZE];
/// 数据区最小容量（连同 4 KiB 头区，文件至少 8 KiB）。
pub(super) const MIN_DATA_CAPACITY: u64 = 4096;
/// 最小文件总大小。参数校验与打开文件都以它为准，避免几处各算一遍。
pub const MIN_TOTAL_SIZE: u64 = HEADER_PAGE + MIN_DATA_CAPACITY;

/// 文件魔数：标识「这是 ringlog 文件」，不含版本含义。
const MAGIC: [u8; 8] = *b"RINGLOG\0";
/// 本程序能读写的文件格式版本。
pub(super) const VERSION: u32 = 1;
/// crc 字段的偏移，同时也是 crc 覆盖范围的终点。
const CRC_OFFSET: usize = 60;

/// 由文件总大小推导数据区容量；不满足最小几何要求时返回 `None`。
pub(super) fn data_capacity(total_size: u64) -> Option<u64> {
    total_size
        .checked_sub(HEADER_PAGE)
        .filter(|capacity| *capacity >= MIN_DATA_CAPACITY)
}

/// 由数据区容量推导文件总大小。
pub(super) fn file_size(capacity: u64) -> u64 {
    HEADER_PAGE + capacity
}

/// slot 解析失败的原因。
///
/// 把「不是 ringlog」「版本不支持」「内容损坏」分开，上层才能给出可操作的提示，
/// 而不是一律报「文件已损坏」让用户去删文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeaderError {
    /// magic 不匹配：这个文件根本不是 ringlog 文件。
    NotRinglog,
    /// magic 匹配，但格式版本不受本程序支持（典型场景：旧程序读到新版程序写的文件）。
    UnsupportedVersion { found: u32, supported: u32 },
    /// magic、版本都匹配，但内容不自洽：CRC 不符、slot 大小不对或位置不变量不成立。
    Corrupt,
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRinglog => f.write_str("魔数不匹配"),
            Self::UnsupportedVersion { found, supported } => {
                write!(f, "文件格式版本为 {found}，本程序只支持版本 {supported}")
            }
            Self::Corrupt => f.write_str("CRC 校验失败或字段不自洽"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub seq: u64,
    pub capacity: u64,
    pub write_pos: u64,
    pub total_written: u64,
}

impl Header {
    pub(super) fn new(seq: u64, capacity: u64) -> Self {
        Self {
            seq,
            capacity,
            write_pos: 0,
            total_written: 0,
        }
    }

    pub(super) fn to_slot(self) -> [u8; SLOT_BYTES] {
        let mut buf = [0u8; SLOT_BYTES];
        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..12].copy_from_slice(&VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(SLOT_SIZE as u32).to_le_bytes());
        buf[16..24].copy_from_slice(&self.seq.to_le_bytes());
        buf[24..32].copy_from_slice(&self.capacity.to_le_bytes());
        buf[32..40].copy_from_slice(&self.write_pos.to_le_bytes());
        buf[40..48].copy_from_slice(&self.total_written.to_le_bytes());
        let crc = crc32c(&buf[..CRC_OFFSET]);
        buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// 解析 slot。
    ///
    /// 校验顺序固定为 magic → version → slot_size → CRC → 位置不变量：版本判断不能依赖
    /// 本版本定义的 CRC 范围，否则旧程序读到新格式时会先撞上 CRC 错误、掩盖真正的原因。
    pub(super) fn from_slot(buf: &[u8; SLOT_BYTES]) -> Result<Self, HeaderError> {
        if buf[..8] != MAGIC[..] {
            return Err(HeaderError::NotRinglog);
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().expect("slot 布局固定"));
        if version != VERSION {
            return Err(HeaderError::UnsupportedVersion {
                found: version,
                supported: VERSION,
            });
        }
        let slot_size = u32::from_le_bytes(buf[12..16].try_into().expect("slot 布局固定"));
        if slot_size != SLOT_SIZE as u32 {
            return Err(HeaderError::Corrupt);
        }
        let stored_crc = u32::from_le_bytes(
            buf[CRC_OFFSET..CRC_OFFSET + 4]
                .try_into()
                .expect("slot 布局固定"),
        );
        if crc32c(&buf[..CRC_OFFSET]) != stored_crc {
            return Err(HeaderError::Corrupt);
        }
        let seq = u64::from_le_bytes(buf[16..24].try_into().expect("slot 布局固定"));
        let capacity = u64::from_le_bytes(buf[24..32].try_into().expect("slot 布局固定"));
        let write_pos = u64::from_le_bytes(buf[32..40].try_into().expect("slot 布局固定"));
        let total_written = u64::from_le_bytes(buf[40..48].try_into().expect("slot 布局固定"));
        if capacity == 0 || write_pos >= capacity || total_written % capacity != write_pos {
            return Err(HeaderError::Corrupt);
        }
        Ok(Self {
            seq,
            capacity,
            write_pos,
            total_written,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        // RFC 3720 附录 B.4 的测试向量
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn header_roundtrip() {
        let h = Header {
            seq: 42,
            capacity: 65536,
            write_pos: 1234,
            total_written: 65536 * 3 + 1234,
        };
        let slot = h.to_slot();
        assert_eq!(Header::from_slot(&slot), Ok(h));
    }

    #[test]
    fn header_rejects_torn_slot() {
        let h = Header::new(7, 4096);
        let mut slot = h.to_slot();
        slot[24] ^= 0xff;
        assert_eq!(Header::from_slot(&slot), Err(HeaderError::Corrupt));
    }

    #[test]
    fn header_rejects_inconsistent_positions() {
        let mut h = Header::new(7, 4096);
        h.write_pos = 100;
        // total_written 不满足不变量
        assert_eq!(h.to_slot().len(), SLOT_BYTES);
        assert_eq!(Header::from_slot(&h.to_slot()), Err(HeaderError::Corrupt));
    }

    #[test]
    fn header_rejects_foreign_magic() {
        let mut slot = Header::new(1, 4096).to_slot();
        slot[0] = b'X';
        // 魔数判断先于 CRC：垃圾文件应报「不是 ringlog」，而不是「损坏」。
        assert_eq!(Header::from_slot(&slot), Err(HeaderError::NotRinglog));
    }

    #[test]
    fn header_reports_future_version() {
        let mut slot = Header::new(1, 4096).to_slot();
        slot[8..12].copy_from_slice(&2u32.to_le_bytes());
        // 故意不重算 CRC：版本判断先于 CRC，否则新版本的 CRC 布局会掩盖真正的原因。
        assert_eq!(
            Header::from_slot(&slot),
            Err(HeaderError::UnsupportedVersion {
                found: 2,
                supported: VERSION,
            })
        );
    }
}
