//! `rustc` and `cargo` diagnostics extractors.
//!
//! These parse diagnostics from the respective stderr JSON output using the
//! data structures defined in [`cargo_metadata::diagnostic`].

use super::{Diagnostics, Level, Message};
use bstr::ByteSlice;
use cargo_metadata::diagnostic::{Diagnostic, DiagnosticLevel, DiagnosticSpan};
use regex::Regex;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

fn diag_line(diag: &Diagnostic, file: &Path) -> Option<(spanned::Span, usize)> {
    let span = |primary| {
        diag.spans
            .iter()
            .find_map(|span| span_line(span, file, primary))
    };
    span(true).or_else(|| span(false))
}

/// Put the message and its children into the line-indexed list.
fn insert_recursive(
    diag: Diagnostic,
    file: &Path,
    messages: &mut Vec<Vec<Message>>,
    messages_from_unknown_file_or_line: &mut Vec<Message>,
    line: Option<(spanned::Span, usize)>,
) {
    let line = diag_line(&diag, file).or(line);
    let msg = Message {
        level: diag.level.into(),
        message: diag.message,
        line: line.as_ref().map(|&(_, l)| l),
        span: line.as_ref().map(|(s, _)| s.clone()),
        code: diag.code.map(|x| x.code),
    };
    if let Some((_, line)) = line.clone() {
        if messages.len() <= line {
            messages.resize_with(line + 1, Vec::new);
        }
        messages[line].push(msg);
    // Summary diagnostics only count diagnostics already reported above.
    } else if !is_diagnostic_summary(&msg.message) {
        messages_from_unknown_file_or_line.push(msg);
    }
    for child in diag.children {
        insert_recursive(
            child,
            file,
            messages,
            messages_from_unknown_file_or_line,
            line.clone(),
        )
    }
}

fn is_diagnostic_summary(message: &str) -> bool {
    static SUMMARY_RE: OnceLock<Regex> = OnceLock::new();
    SUMMARY_RE
        .get_or_init(|| {
            Regex::new(r"aborting due to \d+ previous errors?|\d+ warnings? emitted").unwrap()
        })
        .is_match(message)
}

/// Returns the most expanded line number *in the given file*, if possible.
fn span_line(span: &DiagnosticSpan, file: &Path, primary: bool) -> Option<(spanned::Span, usize)> {
    let file_name = PathBuf::from(&span.file_name);
    if let Some(exp) = &span.expansion {
        if let Some(line) = span_line(&exp.span, file, !primary || span.is_primary) {
            return Some(line);
        } else if file_name != file {
            return if !primary && span.is_primary {
                span_line(&exp.span, file, false)
            } else {
                None
            };
        }
    }
    ((!primary || span.is_primary) && file_name == file).then(|| {
        let span = || {
            Some((
                spanned::Span {
                    file: file_name,
                    bytes: usize::try_from(span.byte_start).unwrap()
                        ..usize::try_from(span.byte_end).unwrap(),
                },
                span.line_start,
            ))
        };
        span().unwrap_or_default()
    })
}

fn filter_annotations_from_rendered(rendered: &str) -> std::borrow::Cow<'_, str> {
    static ANNOTATIONS_RE: OnceLock<Regex> = OnceLock::new();
    ANNOTATIONS_RE
        .get_or_init(|| Regex::new(r" *//(\[[a-z,]+\])?~.*").unwrap())
        .replace_all(rendered, "")
}

/// `rustc` diagnostics extractor.
pub fn rustc_diagnostics_extractor(file: &Path, stderr: &[u8]) -> Diagnostics {
    let mut rendered = Vec::new();
    let mut messages = vec![];
    let mut messages_from_unknown_file_or_line = vec![];
    for line in stderr.lines_with_terminator() {
        if line.starts_with_str(b"{") {
            let msg =
                serde_json::from_slice::<cargo_metadata::diagnostic::Diagnostic>(line).unwrap();

            rendered.extend(
                filter_annotations_from_rendered(msg.rendered.as_ref().unwrap()).as_bytes(),
            );
            insert_recursive(
                msg,
                file,
                &mut messages,
                &mut messages_from_unknown_file_or_line,
                None,
            );
        } else {
            // FIXME: do we want to throw interpreter stderr into a separate file?
            rendered.extend(line);
        }
    }
    Diagnostics {
        rendered,
        messages,
        messages_from_unknown_file_or_line,
    }
}

/// `cargo` diagnostics extractor.
pub fn cargo_diagnostics_extractor(file: &Path, stderr: &[u8]) -> Diagnostics {
    let mut rendered = Vec::new();
    let mut messages = vec![];
    let mut messages_from_unknown_file_or_line = vec![];
    for message in cargo_metadata::Message::parse_stream(stderr) {
        match message.unwrap() {
            cargo_metadata::Message::CompilerMessage(msg) => {
                let msg = msg.message;
                rendered.extend(
                    filter_annotations_from_rendered(msg.rendered.as_ref().unwrap()).as_bytes(),
                );
                insert_recursive(
                    msg,
                    file,
                    &mut messages,
                    &mut messages_from_unknown_file_or_line,
                    None,
                );
            }
            cargo_metadata::Message::TextLine(line) => {
                rendered.extend(line.bytes());
                rendered.push(b'\n')
            }
            _ => {}
        }
    }
    Diagnostics {
        rendered,
        messages,
        messages_from_unknown_file_or_line,
    }
}

impl From<DiagnosticLevel> for Level {
    fn from(value: DiagnosticLevel) -> Self {
        match value {
            DiagnosticLevel::Ice => Level::Ice,
            DiagnosticLevel::Error => Level::Error,
            DiagnosticLevel::Warning => Level::Warn,
            DiagnosticLevel::FailureNote => Level::FailureNote,
            DiagnosticLevel::Note => Level::Note,
            DiagnosticLevel::Help => Level::Help,
            other => panic!("rustc got a new kind of diagnostic level: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_summaries() {
        assert!(is_diagnostic_summary("aborting due to 1 previous error"));
        assert!(is_diagnostic_summary("aborting due to 2 previous errors"));
        assert!(is_diagnostic_summary("1 warning emitted"));
        assert!(is_diagnostic_summary("2 warnings emitted"));
        assert!(is_diagnostic_summary(
            "aborting due to 1 previous error; 2 warnings emitted"
        ));
        assert!(!is_diagnostic_summary("warning emitted by this expression"));
    }
}
