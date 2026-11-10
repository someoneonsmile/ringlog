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
//! 0..8    magic  b"RINGLOG1"
//! 8..12   version = 1
//! 12..16  slot_size = 64
//! 16..24  capacity        数据区容量（字节）
//! 24..32  write_pos       下一次写入在数据区内的偏移
//! 32..40  total_written   累计写入数据区（含环绕填充）的字节数
//! 40..48  seq             每次更新 +1，决定 slot 新旧与奇偶
//! 48..56  epoch           文件“重置纪元”：初始化/--force 重建时 +1，用于读者识别重置
//! 56..60  crc32c(buf[0..56])
//! 60..64  reserved        置 0
//! ```
//!
//! 不变量：`total_written % capacity == write_pos`。

/// CRC32C（Castagnoli）校验，见 RFC 3720 B.4。
pub use crc32c::crc32c;

pub const HEADER_PAGE: u64 = 4096;
pub const DATA_OFFSET: u64 = HEADER_PAGE;
pub const SLOT_SIZE: u64 = 64;
pub const SLOT_BYTES: usize = SLOT_SIZE as usize;
pub const SLOT_OFFSETS: [u64; 2] = [0, SLOT_SIZE];
/// 数据区最小容量（连同 4 KiB 头区，文件至少 8 KiB）。
pub const MIN_DATA_CAPACITY: u64 = 4096;

const MAGIC: u64 = u64::from_le_bytes(*b"RINGLOG1");
const VERSION: u32 = 1;
/// crc 覆盖的字节范围终点。
const CRC_RANGE_END: usize = 56;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub seq: u64,
    pub epoch: u64,
    pub capacity: u64,
    pub write_pos: u64,
    pub total_written: u64,
}

impl Header {
    pub fn new(seq: u64, epoch: u64, capacity: u64) -> Self {
        Self {
            seq,
            epoch,
            capacity,
            write_pos: 0,
            total_written: 0,
        }
    }

    pub fn to_slot(self) -> [u8; SLOT_BYTES] {
        let mut buf = [0u8; SLOT_BYTES];
        buf[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        buf[8..12].copy_from_slice(&VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(SLOT_SIZE as u32).to_le_bytes());
        buf[16..24].copy_from_slice(&self.capacity.to_le_bytes());
        buf[24..32].copy_from_slice(&self.write_pos.to_le_bytes());
        buf[32..40].copy_from_slice(&self.total_written.to_le_bytes());
        buf[40..48].copy_from_slice(&self.seq.to_le_bytes());
        buf[48..56].copy_from_slice(&self.epoch.to_le_bytes());
        let crc = crc32c(&buf[..CRC_RANGE_END]);
        buf[56..60].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// 解析 slot；任何不自洽（magic/version/CRC/不变量）都返回 `None`。
    pub fn from_slot(buf: &[u8; SLOT_BYTES]) -> Option<Self> {
        let stored_crc = u32::from_le_bytes(buf[56..60].try_into().ok()?);
        if crc32c(&buf[..CRC_RANGE_END]) != stored_crc {
            return None;
        }
        let magic = u64::from_le_bytes(buf[0..8].try_into().ok()?);
        if magic != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().ok()?);
        if version != VERSION {
            return None;
        }
        let slot_size = u32::from_le_bytes(buf[12..16].try_into().ok()?);
        if slot_size != SLOT_SIZE as u32 {
            return None;
        }
        let capacity = u64::from_le_bytes(buf[16..24].try_into().ok()?);
        let write_pos = u64::from_le_bytes(buf[24..32].try_into().ok()?);
        let total_written = u64::from_le_bytes(buf[32..40].try_into().ok()?);
        let seq = u64::from_le_bytes(buf[40..48].try_into().ok()?);
        let epoch = u64::from_le_bytes(buf[48..56].try_into().ok()?);
        if capacity == 0 || write_pos >= capacity || total_written % capacity != write_pos {
            return None;
        }
        Some(Self {
            seq,
            epoch,
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
            epoch: 7,
            capacity: 65536,
            write_pos: 1234,
            total_written: 65536 * 3 + 1234,
        };
        let slot = h.to_slot();
        assert_eq!(Header::from_slot(&slot), Some(h));
    }

    #[test]
    fn header_rejects_torn_slot() {
        let h = Header::new(7, 1, 4096);
        let mut slot = h.to_slot();
        slot[24] ^= 0xff;
        assert_eq!(Header::from_slot(&slot), None);
    }

    #[test]
    fn header_rejects_inconsistent_positions() {
        let mut h = Header::new(7, 1, 4096);
        h.write_pos = 100;
        // total_written 不满足不变量
        assert_eq!(h.to_slot().len(), SLOT_BYTES);
        assert_eq!(Header::from_slot(&h.to_slot()), None);
    }
}
