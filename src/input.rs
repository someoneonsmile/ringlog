//! 把字节流切成「以换行结尾」的记录。
//!
//! - 每条记录长度不超过数据区容量（含换行符）；
//! - 超长行截断，保留行首（`capacity - 1` 字节）+ 换行，并计数告警；
//! - 输入末尾缺少换行符时补一个，保证每条记录都以 `\n` 结尾。

/// 单条记录长度上限 = 数据区容量（含结尾换行），据此决定行首保留字节数。
#[derive(Debug)]
pub struct LineSplitter {
    max_record: usize,
    payload: Vec<u8>,
    records: Vec<(usize, usize)>,
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
            records: Vec::new(),
            carry: Vec::new(),
            truncated: false,
            truncated_lines: 0,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let mut last = 0;
        for i in memchr::memchr_iter(b'\n', chunk) {
            self.push_carry(&chunk[last..i]);
            self.end_record();
            last = i + 1;
        }
        // 末尾没有换行符的半行留在 carry 里等下一块数据。
        self.push_carry(&chunk[last..]);
    }

    /// 输入结束：把没有换行符收尾的最后一行补上 `\n`。
    pub fn finish(&mut self) {
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
        let start = self.payload.len();
        self.payload.extend_from_slice(&self.carry);
        self.payload.push(b'\n');
        self.records.push((start, self.payload.len() - start));
        self.carry.clear();
        if self.truncated {
            self.truncated_lines += 1;
            self.truncated = false;
        }
    }

    /// 取走已经成形的记录（未结束的半行留在内部）。
    pub fn take(&mut self) -> (Vec<u8>, Vec<(usize, usize)>) {
        (
            std::mem::take(&mut self.payload),
            std::mem::take(&mut self.records),
        )
    }

    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }

    pub fn records_len(&self) -> usize {
        self.records.len()
    }

    pub fn truncated_lines(&self) -> u64 {
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
        let (payload, records) = {
            splitter.feed(input);
            splitter.finish();
            splitter.take()
        };
        records
            .iter()
            .map(|&(start, len)| payload[start..start + len].to_vec())
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
        let (payload, records) = splitter.take();
        assert_eq!(payload, b"hello\nworld\n".to_vec());
        assert_eq!(records, vec![(0, 6), (6, 6)]);
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
    fn max_record_len_is_capacity() {
        let splitter = LineSplitter::new(8);
        assert_eq!(splitter.max_record(), 8);
    }
}
