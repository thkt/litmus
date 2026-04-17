use litmus::{analyze_files, find_test_files};
use std::env;
use std::path::Path;
use std::process;

fn main() {
    let dir = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let dir = Path::new(&dir);

    let files = find_test_files(dir);
    let result = analyze_files(&files);

    for error in &result.errors {
        eprintln!("{error}");
    }

    if result.issues.is_empty() {
        process::exit(0);
    }

    for issue in &result.issues {
        println!("{issue}");
    }

    process::exit(1);
}
