pub mod parse;
pub mod rules;

use parse::parse_test_file;
use rules::{
    Issue, check_catch_only_assertion, check_catch_swallow, check_conditional_assertion,
    check_dummy_data, check_empty_test, check_mock_only, check_mock_overuse, check_skipped_test,
    check_tautological, check_test_name, check_weak_assertions,
};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const EXCLUDED_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "target"];

// Exit codes per ADR-0066 Group 3 (Hook tool).
//
// Adopted:
//   0  EX_OK         clean (no violations)
//   1  (reserved)    advisory; reserved for future warn-level rules
//   2  (convention)  blocking failure (violations detected)
//   64 EX_USAGE      bad command-line usage
//   70 EX_SOFTWARE   internal error (panic / invariant violation)
//
// Not adopted (ADR-0066 Confirmation requires reasons):
//   65 EX_DATAERR    input is a dir path only; no malformed-data concept
//   73 EX_CANTCREAT  litmus does not create output files
//   74 EX_IOERR      per-file read errors are reported to stderr and skipped;
//                    no aggregate IO-failure exit
//   75 EX_TEMPFAIL   no retryable failure mode (local, deterministic analysis)
//   104 UNKNOWN      anyhow::Error swallow fallback; litmus does not use anyhow
pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_WARNING: u8 = 1;
pub const EXIT_BLOCKING: u8 = 2;
pub const EXIT_USAGE: u8 = 64;
pub const EXIT_SOFTWARE: u8 = 70;

#[derive(Debug)]
pub enum LitmusError {
    Usage(String),
    Internal(String),
}

impl LitmusError {
    pub fn exit_code(&self) -> u8 {
        match self {
            LitmusError::Usage(_) => EXIT_USAGE,
            LitmusError::Internal(_) => EXIT_SOFTWARE,
        }
    }
}

impl fmt::Display for LitmusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LitmusError::Usage(msg) => write!(f, "litmus: usage error: {msg}"),
            LitmusError::Internal(msg) => write!(f, "litmus: internal error: {msg}"),
        }
    }
}

#[derive(Debug)]
pub struct FileError {
    pub file: PathBuf,
    pub kind: FileErrorKind,
    pub message: String,
}

#[derive(Debug)]
pub enum FileErrorKind {
    Read,
    Parse,
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            FileErrorKind::Read => "read error",
            FileErrorKind::Parse => "parse error",
        };
        write!(
            f,
            "litmus: {kind}: {}: {}",
            self.file.display(),
            self.message
        )
    }
}

pub struct AnalysisResult {
    pub issues: Vec<Issue>,
    pub errors: Vec<FileError>,
}

pub fn find_test_files(dir: &Path) -> Vec<PathBuf> {
    let patterns = [dir.join("**/*.test.ts"), dir.join("**/*.test.tsx")];

    let mut files = Vec::new();
    for pattern in &patterns {
        let Some(pat) = pattern.to_str() else {
            continue;
        };
        if let Ok(paths) = glob::glob(pat) {
            for entry in paths.flatten() {
                if !is_excluded(&entry) {
                    files.push(entry);
                }
            }
        }
    }
    files
}

fn is_excluded(path: &Path) -> bool {
    path.components()
        .any(|c| EXCLUDED_DIRS.contains(&c.as_os_str().to_str().unwrap_or("")))
}

pub fn analyze_files(files: &[PathBuf]) -> AnalysisResult {
    let mut issues = Vec::new();
    let mut errors = Vec::new();

    for file in files {
        let source = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                errors.push(FileError {
                    file: file.clone(),
                    kind: FileErrorKind::Read,
                    message: e.to_string(),
                });
                continue;
            }
        };

        let blocks = match parse_test_file(&source, file) {
            Ok(blocks) => blocks,
            Err(e) => {
                errors.push(FileError {
                    file: file.clone(),
                    kind: FileErrorKind::Parse,
                    message: e.to_string(),
                });
                continue;
            }
        };

        issues.extend(check_empty_test(&blocks, file));
        issues.extend(check_skipped_test(&blocks, file));
        issues.extend(check_catch_swallow(&blocks, file));
        issues.extend(check_conditional_assertion(&blocks, file));
        issues.extend(check_catch_only_assertion(&blocks, file));
        issues.extend(check_weak_assertions(&blocks, file));
        issues.extend(check_mock_overuse(&blocks, file));
        issues.extend(check_tautological(&blocks, file));
        issues.extend(check_mock_only(&blocks, file));
        issues.extend(check_test_name(&blocks, file));
        issues.extend(check_dummy_data(&blocks, file));
    }

    AnalysisResult { issues, errors }
}

#[cfg(test)]
mod error_tests {
    use super::{
        EXIT_BLOCKING, EXIT_SOFTWARE, EXIT_SUCCESS, EXIT_USAGE, EXIT_WARNING, LitmusError,
    };

    // T-401, T-406: ADR-0066 Group 3 exit code constants
    #[test]
    fn exit_codes_pinned_to_adr_0066_group_3() {
        assert_eq!(EXIT_SUCCESS, 0);
        assert_eq!(EXIT_WARNING, 1);
        assert_eq!(EXIT_BLOCKING, 2);
        assert_eq!(EXIT_USAGE, 64);
        assert_eq!(EXIT_SOFTWARE, 70);
    }

    // T-402: Usage variant maps to 64
    #[test]
    fn usage_exit_code_is_64() {
        let e = LitmusError::Usage("too many args".to_owned());
        assert_eq!(e.exit_code(), EXIT_USAGE);
    }

    // T-403: Internal variant maps to 70
    #[test]
    fn internal_exit_code_is_70() {
        let e = LitmusError::Internal("invariant violated".to_owned());
        assert_eq!(e.exit_code(), EXIT_SOFTWARE);
    }

    // T-404: Display includes the underlying message
    #[test]
    fn display_usage_includes_message() {
        let e = LitmusError::Usage("unknown flag --foo".to_owned());
        assert_eq!(e.to_string(), "litmus: usage error: unknown flag --foo");
    }

    // T-405: Display distinguishes internal from usage
    #[test]
    fn display_internal_includes_message() {
        let e = LitmusError::Internal("invariant: empty path".to_owned());
        assert_eq!(
            e.to_string(),
            "litmus: internal error: invariant: empty path"
        );
    }
}
