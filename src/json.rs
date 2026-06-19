//! Hand-rolled JSON serialization for the `--json` output mode.
//!
//! litmus runs on the hook path (gates embedding), where every added
//! dependency raises startup cost. The output schema is small and fixed, so
//! serialization is hand-written rather than pulling in serde (OUTCOME.md
//! dependency constraint). Correctness rests on `escape` covering every JSON
//! string hazard; the unit tests below pin that contract.

use crate::rules::{Issue, Severity};
use crate::{AnalysisResult, FileError, FileErrorKind, LitmusError};

/// Escape a string for embedding inside a JSON string literal (RFC 8259).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn severity_str(severity: Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Blocking => "blocking",
    }
}

fn render_issue(issue: &Issue) -> String {
    format!(
        "{{\"rule\":\"{}\",\"severity\":\"{}\",\"file\":\"{}\",\"line\":{},\"test_name\":\"{}\",\"detail\":\"{}\"}}",
        escape(issue.rule),
        severity_str(issue.severity()),
        escape(&issue.file.display().to_string()),
        issue.line,
        escape(&issue.test_name),
        escape(&issue.detail),
    )
}

fn render_file_error(error: &FileError) -> String {
    let kind = match error.kind {
        FileErrorKind::Read => "read",
        FileErrorKind::Parse => "parse",
        FileErrorKind::Crash => "crash",
    };
    format!(
        "{{\"file\":\"{}\",\"kind\":\"{}\",\"message\":\"{}\"}}",
        escape(&error.file.display().to_string()),
        kind,
        escape(&error.message),
    )
}

/// Render the issues and errors arrays as their inner comma-joined fragments,
/// without the enclosing `{"issues":[...],"errors":[...]}` wrapper. The parent
/// process merges per-file worker fragments into one document; both fragments
/// are newline-free because `escape` strips every control char (incl. \n, \r),
/// which lets the worker frame them as `issues_frag\nerrors_frag`.
pub fn render_fragments(result: &AnalysisResult) -> (String, String) {
    let issues = result
        .issues
        .iter()
        .map(render_issue)
        .collect::<Vec<_>>()
        .join(",");
    let errors = result
        .errors
        .iter()
        .map(render_file_error)
        .collect::<Vec<_>>()
        .join(",");
    (issues, errors)
}

/// Render the full analysis result as a single JSON document for stdout.
pub fn render_result(result: &AnalysisResult) -> String {
    let (issues, errors) = render_fragments(result);
    format!("{{\"issues\":[{issues}],\"errors\":[{errors}]}}")
}

/// Render a CLI error as JSON for stderr, carrying `next_step` + `candidates`
/// so an agent can recover without parsing the human message.
pub fn render_error(err: &LitmusError) -> String {
    let (kind, message, next_step, candidates): (_, _, _, &[&str]) = match err {
        LitmusError::Usage(msg) => (
            "usage",
            msg.as_str(),
            "Pass at most one directory path; the only accepted flag is --json.",
            &["--json"],
        ),
        LitmusError::Internal(msg) => (
            "internal",
            msg.as_str(),
            "Rerun to confirm reproducibility, then report it as a litmus bug.",
            &[],
        ),
    };
    let cands = candidates
        .iter()
        .map(|c| format!("\"{}\"", escape(c)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"error\":\"{kind}\",\"message\":\"{}\",\"next_step\":\"{}\",\"candidates\":[{cands}]}}",
        escape(message),
        escape(next_step),
    )
}

#[cfg(test)]
mod tests {
    use super::{escape, render_error, render_result};
    use crate::rules::Issue;
    use crate::{AnalysisResult, FileError, FileErrorKind, LitmusError};
    use std::path::PathBuf;

    // T-J01: control chars, quotes, and backslashes are escaped per RFC 8259
    #[test]
    fn escape_covers_json_string_hazards() {
        assert_eq!(escape("a\"b"), "a\\\"b");
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(escape("a\tb"), "a\\tb");
        assert_eq!(escape("a\u{01}b"), "a\\u0001b");
    }

    // T-J02: empty result renders empty arrays, not null
    #[test]
    fn render_result_empty_has_empty_arrays() {
        let result = AnalysisResult {
            issues: Vec::new(),
            errors: Vec::new(),
        };
        assert_eq!(render_result(&result), r#"{"issues":[],"errors":[]}"#);
    }

    // T-J03: one blocking issue renders with severity and escaped fields
    #[test]
    fn render_result_one_issue() {
        let result = AnalysisResult {
            issues: vec![Issue {
                rule: "weak-assertion",
                file: PathBuf::from("a.test.ts"),
                line: 7,
                test_name: "checks \"x\"".to_owned(),
                detail: String::new(),
            }],
            errors: Vec::new(),
        };
        assert_eq!(
            render_result(&result),
            r#"{"issues":[{"rule":"weak-assertion","severity":"blocking","file":"a.test.ts","line":7,"test_name":"checks \"x\"","detail":""}],"errors":[]}"#
        );
    }

    // T-J04: file errors carry kind discriminant
    #[test]
    fn render_result_with_file_error() {
        let result = AnalysisResult {
            issues: Vec::new(),
            errors: vec![FileError {
                file: PathBuf::from("b.test.ts"),
                kind: FileErrorKind::Parse,
                message: "unexpected token".to_owned(),
            }],
        };
        assert_eq!(
            render_result(&result),
            r#"{"issues":[],"errors":[{"file":"b.test.ts","kind":"parse","message":"unexpected token"}]}"#
        );
    }

    // T-J15: a worker crash carries the "crash" kind discriminant, distinct from
    // "parse", so a consumer can tell "litmus could not analyze" from "the file
    // is malformed".
    #[test]
    fn render_result_with_crash_error() {
        let result = AnalysisResult {
            issues: Vec::new(),
            errors: vec![FileError {
                file: PathBuf::from("c.test.ts"),
                kind: FileErrorKind::Crash,
                message: "analysis aborted: worker terminated by signal".to_owned(),
            }],
        };
        assert_eq!(
            render_result(&result),
            r#"{"issues":[],"errors":[{"file":"c.test.ts","kind":"crash","message":"analysis aborted: worker terminated by signal"}]}"#
        );
    }

    // T-J05: usage error JSON carries next_step + candidate flag
    #[test]
    fn render_error_usage_has_next_step_and_candidates() {
        let e = LitmusError::Usage("unknown flag: --foo".to_owned());
        assert_eq!(
            render_error(&e),
            r#"{"error":"usage","message":"unknown flag: --foo","next_step":"Pass at most one directory path; the only accepted flag is --json.","candidates":["--json"]}"#
        );
    }

    // T-J06: internal error JSON has empty candidates
    #[test]
    fn render_error_internal_has_empty_candidates() {
        let e = LitmusError::Internal("unexpected panic".to_owned());
        assert_eq!(
            render_error(&e),
            r#"{"error":"internal","message":"unexpected panic","next_step":"Rerun to confirm reproducibility, then report it as a litmus bug.","candidates":[]}"#
        );
    }
}
