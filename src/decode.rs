//! 命令输出解码：按行判断编码。合法 UTF-8 直接使用；否则在 Windows 上按 OEM 代码页
//! （中文系统为 GBK）解码，其他平台做有损 UTF-8。逐行判断可以容忍 UTF-8 与 GBK 混杂的输出。

use encoding_rs::Encoding;

pub struct StreamDecoder {
    pending: Vec<u8>,
    fallback: Option<&'static Encoding>,
}

const FLUSH_LIMIT: usize = 16 * 1024;

impl StreamDecoder {
    pub fn new(fallback: Option<&'static Encoding>) -> Self {
        Self {
            pending: Vec::new(),
            fallback,
        }
    }

    /// 追加字节，返回已完整的行（含换行符）。
    pub fn push(&mut self, data: &[u8]) -> String {
        self.pending.extend_from_slice(data);
        let mut out = match self.pending.iter().rposition(|&b| b == b'\n') {
            Some(i) => {
                let rest = self.pending.split_off(i + 1);
                let done = std::mem::replace(&mut self.pending, rest);
                self.decode_lines(&done)
            }
            None => String::new(),
        };
        if self.pending.len() > FLUSH_LIMIT {
            out.push_str(&self.flush_partial());
        }
        out
    }

    /// 输出静默一段时间时调用：吐出未成行的部分（如进度提示），保留可能被截断的多字节尾巴。
    pub fn flush_partial(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let keep = match std::str::from_utf8(&self.pending) {
            Ok(_) => 0,
            Err(e) if e.error_len().is_none() => self.pending.len() - e.valid_up_to(),
            Err(_) => usize::from(self.pending.last().is_some_and(|b| *b >= 0x80)),
        };
        let cut = self.pending.len() - keep;
        let rest = self.pending.split_off(cut);
        let done = std::mem::replace(&mut self.pending, rest);
        self.decode_one(&done)
    }

    pub fn finish(&mut self) -> String {
        let done = std::mem::take(&mut self.pending);
        self.decode_one(&done)
    }

    fn decode_lines(&self, bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len());
        for line in bytes.split_inclusive(|&b| b == b'\n') {
            out.push_str(&self.decode_one(line));
        }
        out
    }

    fn decode_one(&self, bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return String::new();
        }
        match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => match self.fallback {
                Some(enc) => enc.decode_without_bom_handling(bytes).0.into_owned(),
                None => String::from_utf8_lossy(bytes).into_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_utf8_and_gbk_lines() {
        let mut d = StreamDecoder::new(Some(encoding_rs::GBK));
        let (gbk, _, _) = encoding_rs::GBK.encode("中文\n");
        let mut data = "你好\n".as_bytes().to_vec();
        data.extend_from_slice(&gbk);
        assert_eq!(d.push(&data), "你好\n中文\n");
    }

    #[test]
    fn split_multibyte_kept() {
        let mut d = StreamDecoder::new(None);
        let bytes = "汉字".as_bytes();
        assert_eq!(d.push(&bytes[..4]), "");
        assert_eq!(d.flush_partial(), "汉");
        assert_eq!(d.push(&bytes[4..]), "");
        assert_eq!(d.finish(), "字");
    }
}
