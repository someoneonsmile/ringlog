//! 把字节流切成「以换行结尾」的记录。
//!
//! - 每条记录长度不超过数据区容量（含换行符）；
//! - 超长行截断，保留行首（`capacity - 1` 字节）+ 换行，并计数告警；
//! - 输入末尾缺少换行符时补一个，保证每条记录都以 `\n` 结尾。
//!
//! 输出的 payload 保证「一串以 `\n` 结尾、长度不超过数据区容量的记录」，
//! 但不登记记录边界：只有写入跨越文件尾时才需要按记录切分，交给 `ring` 扫描更省。

use memchr::memchr_iter;

/// 单条记录长度上限 = 数据区容量（含结尾换行），据此决定行首保留字节数。
#[derive(Debug)]
pub struct LineSplitter {
    max_record: usize,
    payload: Vec<u8>,
    /// 当前行尚未结束的字节。
    carry: Vec<u8>,
    /// 当前行是否已超长、后续字节被丢弃。
    truncated: bool,
    truncated_lines: u64,
}

impl LineSplitter {
    pub fn new(capacity: u64) -> Self {
        // 记录长度 ≥ 2（至少 1 字节内容 + 换行）
        let max_record = capacity.max(2) as usize;
        Self {
            max_record,
            payload: Vec::new(),
            carry: Vec::new(),
            truncated: false,
            truncated_lines: 0,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        // 快路径：上一块没有残行、也没有进行中的截断时，整块拷贝一次。
        if self.carry.is_empty() && !self.truncated && self.feed_complete(chunk) {
            return;
        }
        self.feed_line_by_line(chunk);
    }

    /// 快路径：块内记录都不超长时，整块一次性拷贝；末尾半行收进 carry。
    /// 返回 `false` 表示需要走逐行路径（有超长行或末尾半行放不下）。
    fn feed_complete(&mut self, chunk: &[u8]) -> bool {
        let mut start = 0;
        for i in memchr_iter(b'\n', chunk) {
            if i + 1 - start > self.max_record {
                return false;
            }
            start = i + 1;
        }
        // 末尾没有换行符的半行先收进 carry；放不下说明要截断，交给逐行路径。
        let tail = &chunk[start..];
        if tail.len() > self.max_record - 1 {
            return false;
        }
        self.payload.extend_from_slice(&chunk[..start]);
        self.carry.extend_from_slice(tail);
        true
    }

    /// 逐行路径：处理跨块残行与超长行截断。
    fn feed_line_by_line(&mut self, chunk: &[u8]) {
        let mut last = 0;
        for i in memchr_iter(b'\n', chunk) {
            self.push_carry(&chunk[last..i]);
            self.end_record();
            last = i + 1;
        }
        // 末尾没有换行符的半行留在 carry 里等下一块数据。
        self.push_carry(&chunk[last..]);
    }

    /// 输入结束：把没有换行符收尾的最后一行补上 `\n`。
    fn finish(&mut self) {
        if self.carry.is_empty() && !self.truncated {
            return;
        }
        self.end_record();
    }

    /// 把一段不含换行符的字节放进 carry；超出记录上限的部分丢弃并标记截断。
    fn push_carry(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let room = self.max_record - 1 - self.carry.len();
        if bytes.len() <= room {
            self.carry.extend_from_slice(bytes);
        } else {
            self.carry.extend_from_slice(&bytes[..room]);
            self.truncated = true;
        }
    }

    /// 收尾当前行：carry + `\n` 组成一条完整记录。
    fn end_record(&mut self) {
        self.payload.extend_from_slice(&self.carry);
        self.payload.push(b'\n');
        self.carry.clear();
        if self.truncated {
            self.truncated_lines += 1;
            self.truncated = false;
        }
    }

    /// 取走已经成形的记录。`final_chunk` 表示输入已结束：把没有换行符的最后一行
    /// 补上 `\n` 一起取走；否则未结束的半行留在内部等下一块数据。
    pub fn take(&mut self, final_chunk: bool) -> Vec<u8> {
        if final_chunk {
            self.finish();
        }
        std::mem::take(&mut self.payload)
    }

    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }

    /// 仅供测试断言截断计数；生产代码只用 [`Self::take_truncated_lines`]。
    #[cfg(test)]
    pub(super) fn truncated_lines(&self) -> u64 {
        self.truncated_lines
    }

    pub fn take_truncated_lines(&mut self) -> u64 {
        std::mem::take(&mut self.truncated_lines)
    }

    /// 单条记录的最大长度（字节，含结尾换行）。
    pub fn max_record(&self) -> usize {
        self.max_record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_all(capacity: u64, input: &[u8]) -> Vec<Vec<u8>> {
        let mut splitter = LineSplitter::new(capacity);
        splitter.feed(input);
        splitter
            .take(true)
            .split_inclusive(|&b| b == b'\n')
            .map(<[u8]>::to_vec)
            .collect()
    }

    #[test]
    fn splits_lines_and_keeps_blank_lines() {
        let lines = split_all(4096, b"a\n\nb\n");
        assert_eq!(
            lines,
            vec![b"a\n".to_vec(), b"\n".to_vec(), b"b\n".to_vec()]
        );
    }

    #[test]
    fn appends_missing_final_newline() {
        assert_eq!(split_all(4096, b"abc"), vec![b"abc\n".to_vec()]);
    }

    #[test]
    fn splits_across_chunks() {
        let mut splitter = LineSplitter::new(4096);
        splitter.feed(b"hel");
        splitter.feed(b"lo\nwor");
        splitter.feed(b"ld\n");
        assert_eq!(splitter.take(false), b"hello\nworld\n".to_vec());
    }

    #[test]
    fn carries_partial_line_across_chunks() {
        let mut splitter = LineSplitter::new(4096);
        splitter.feed(b"done\npar");
        assert_eq!(splitter.take(false), b"done\n".to_vec());
        splitter.feed(b"tial\n");
        assert_eq!(splitter.take(false), b"partial\n".to_vec());
    }

    #[test]
    fn truncates_overlong_line_keeping_head() {
        let cap = 8;
        let lines = split_all(cap, b"0123456789abcdef\nok\n");
        assert_eq!(lines, vec![b"0123456\n".to_vec(), b"ok\n".to_vec()]);
        let mut splitter = LineSplitter::new(cap);
        splitter.feed(b"0123456789abcdef\n");
        assert_eq!(splitter.truncated_lines(), 1);
    }

    #[test]
    fn truncation_across_chunks_is_counted_once() {
        let mut splitter = LineSplitter::new(8);
        splitter.feed(b"0123456");
        splitter.feed(b"789\n");
        assert_eq!(splitter.take(false), b"0123456\n".to_vec());
        assert_eq!(splitter.take_truncated_lines(), 1);
    }

    #[test]
    fn line_exactly_max_record_is_kept() {
        let mut splitter = LineSplitter::new(8);
        splitter.feed(b"0123456\n");
        assert_eq!(splitter.take(false), b"0123456\n".to_vec());
        assert_eq!(splitter.truncated_lines(), 0);
    }

    #[test]
    fn non_final_take_keeps_partial_line() {
        let mut splitter = LineSplitter::new(4096);
        splitter.feed(b"done\npar");
        assert_eq!(splitter.take(false), b"done\n".to_vec());
        assert_eq!(splitter.take(true), b"par\n".to_vec());
    }

    #[test]
    fn empty_input_yields_empty_payload() {
        let mut splitter = LineSplitter::new(4096);
        assert!(splitter.take(true).is_empty());
    }

    #[test]
    fn max_record_len_is_capacity() {
        let splitter = LineSplitter::new(8);
        assert_eq!(splitter.max_record(), 8);
    }
}
