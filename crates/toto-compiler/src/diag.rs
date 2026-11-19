//! Diagnostics: spans, line/column, source snippets, stable codes.

use crate::span::Span;

/// A static diagnostic (lexer or parser). Runtime panics reuse this shape
/// with `is_panic` rendering (`panic[CODE]` instead of `error[CODE]`).
#[derive(Clone, Debug)]
pub struct Diag {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
    // NOTE: no origin-file field here by design. `Diag` values ride in
    // `Result` error payloads through the MIR executor's coroutine
    // frames (debug builds spill them per match arm); every extra word
    // costs ~10 KiB of coroutine stack in the giant `exec_instr` /
    // `call_builtin` matches, and that path already uses ~58 KiB of
    // its 64 KiB budget. Multi-file attribution travels out-of-band
    // (`check_with_files`), never on the struct.
}

impl Diag {
    pub fn new(code: &'static str, message: String, span: Span, help: Option<String>) -> Self {
        Self {
            code,
            message,
            span,
            help,
        }
    }

    /// Render one diagnostic in the spec §9.1 format.
    pub fn render(&self, src: &str, filename: &str, is_panic: bool) -> String {
        let kind = if is_panic { "panic" } else { "error" };
        let mut out = format!("{}[{}]: {}\n", kind, self.code, self.message);
        let (line, col) = self.span.line_col(src);
        out.push_str(&format!(" --> {filename}:{line}:{col}\n"));
        let line_text = src.lines().nth(line.saturating_sub(1)).unwrap_or("");
        let width = line.to_string().len();
        out.push_str(&format!("{:width$} |\n", "", width = width));
        out.push_str(&format!("{line} | {line_text}\n"));
        // Caret span: clamp to the line, count in characters.
        // Byte offset of the line start, not `lines()` lengths:
        // `lines()` strips a `\r\n` terminator's `\r`, which put
        // every caret one column right on CRLF sources.
        let line_start = if line <= 1 {
            0
        } else {
            src.match_indices('\n')
                .nth(line - 2)
                .map(|(i, _)| i + 1)
                .unwrap_or(src.len())
        };
        let start_in_line = (self.span.start as usize).saturating_sub(line_start);
        let char_col = line_text
            .char_indices()
            .take_while(|(i, _)| *i < start_in_line.min(line_text.len()))
            .count();
        let span_chars = {
            let rest = &line_text[line_text
                .char_indices()
                .nth(char_col)
                .map(|(i, _)| i)
                .unwrap_or(line_text.len())..];
            let wanted = span_char_len(self.span, src, line_start);
            rest.chars().take(wanted.max(1)).count().max(1)
        };
        out.push_str(&format!(
            "{:width$} | {}{}",
            "",
            " ".repeat(char_col),
            "^".repeat(span_chars),
            width = width
        ));
        if let Some(help) = &self.help {
            out.push_str(&format!(" help: {help}"));
        }
        out.push('\n');
        out
    }
}

fn span_char_len(span: Span, src: &str, line_start: usize) -> usize {
    let end = (span.end as usize).min(src.len());
    let from = (span.start as usize).max(line_start).min(end);
    src[from..end].chars().take_while(|&c| c != '\n').count()
}

/// Render several diagnostics separated by blank lines.
pub fn render_all(diags: &[Diag], src: &str, filename: &str, is_panic: bool) -> String {
    diags
        .iter()
        .map(|d| d.render(src, filename, is_panic))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag_at(start: u32, end: u32) -> Diag {
        Diag::new(
            "E0001",
            "unexpected character `'`".to_string(),
            Span { start, end },
            None,
        )
    }

    #[test]
    fn caret_aligns_identically_on_lf_and_crlf() {
        // `'` at line 2, column 13 in both variants.
        let lf = "fn main() -> int {\n    println('hi');\n    return 0;\n}\n";
        let crlf = "fn main() -> int {\r\n    println('hi');\r\n    return 0;\r\n}\r\n";
        let lf_start = lf.find('\'').expect("quote") as u32;
        let crlf_start = crlf.find('\'').expect("quote") as u32;
        let lf_out = diag_at(lf_start, lf_start + 1).render(lf, "f.gzv", false);
        let crlf_out = diag_at(crlf_start, crlf_start + 1).render(crlf, "f.gzv", false);
        // Same caret line, and no carriage return leaks into output.
        let caret = |out: &str| {
            out.lines()
                .find(|l| l.split('|').nth(1).is_some_and(|r| r.contains('^')))
                .expect("caret line")
                .to_string()
        };
        assert_eq!(caret(&lf_out), caret(&crlf_out));
        assert!(!crlf_out.contains('\r'), "CR leaked: {crlf_out:?}");
    }
}
