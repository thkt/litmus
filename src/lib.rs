pub mod parse;
pub mod rules;

use parse::parse_test_file;
use rules::{
    Issue, check_catch_only_assertion, check_catch_swallow, check_conditional_assertion,
    check_empty_test, check_mock_only, check_mock_overuse, check_skipped_test, check_tautological,
    check_test_name, check_weak_assertions,
};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const EXCLUDED_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "target"];

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
    }

    AnalysisResult { issues, errors }
}
