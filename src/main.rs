mod parse;
mod rules;

use parse::parse_test_file;
use rules::{check_mock_only, check_mock_overuse, check_tautological, check_test_name, check_weak_assertions, Issue};
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let dir = Path::new(&dir);

    let files = find_test_files(dir);
    let issues = analyze_files(&files);

    if issues.is_empty() {
        process::exit(0);
    }

    for issue in &issues {
        println!("{issue}");
    }

    process::exit(1);
}

fn find_test_files(dir: &Path) -> Vec<PathBuf> {
    let patterns = [
        dir.join("**/*.test.ts"),
        dir.join("**/*.test.tsx"),
    ];

    let mut files = Vec::new();
    for pattern in &patterns {
        let Some(pat) = pattern.to_str() else {
            eprintln!("litmus: non-UTF-8 path skipped: {:?}", pattern);
            continue;
        };
        match glob::glob(pat) {
            Ok(paths) => {
                for entry in paths {
                    match entry {
                        Ok(p) if !is_excluded(&p) => files.push(p),
                        Ok(_) => {}
                        Err(e) => eprintln!("litmus: glob error: {e}"),
                    }
                }
            }
            Err(e) => eprintln!("litmus: invalid glob pattern {pat}: {e}"),
        }
    }
    files
}

const EXCLUDED_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "target"];

fn is_excluded(path: &Path) -> bool {
    path.components()
        .any(|c| EXCLUDED_DIRS.contains(&c.as_os_str().to_str().unwrap_or("")))
}

fn analyze_files(files: &[PathBuf]) -> Vec<Issue> {
    let mut issues = Vec::new();

    for file in files {
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("litmus: read error: {}: {e}", file.display());
                continue;
            }
        };

        let blocks = match parse_test_file(&source, file) {
            Ok(blocks) => blocks,
            Err(e) => {
                eprintln!("litmus: parse error: {}: {e}", file.display());
                continue;
            }
        };

        issues.extend(check_weak_assertions(&blocks, file));
        issues.extend(check_mock_overuse(&blocks, file));
        issues.extend(check_tautological(&blocks, file));
        issues.extend(check_mock_only(&blocks, file));
        issues.extend(check_test_name(&blocks, file));
    }

    issues
}
