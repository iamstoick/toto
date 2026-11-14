//! Byte-offset spans. Line/column are computed from the source on demand.

/// A half-open byte range `[start, end)` into the source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// Smallest span covering both.
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// 1-based `(line, column)` of the span start. Column counts characters.
    pub fn line_col(&self, src: &str) -> (usize, usize) {
        let start = (self.start as usize).min(src.len());
        let head = &src[..start];
        let line = head.bytes().filter(|&b| b == b'\n').count() + 1;
        let col = head.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        (line, col)
    }
}
