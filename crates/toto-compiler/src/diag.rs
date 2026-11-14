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
        let line_start = src
            .lines()
            .take(line.saturating_sub(1))
            .map(|l| l.len() + 1)
            .sum::<usize>();
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
