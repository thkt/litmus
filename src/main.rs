use litmus::{EXIT_BLOCKING, EXIT_SUCCESS, LitmusError, analyze_files, find_test_files};
use std::env;
use std::panic::catch_unwind;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    match catch_unwind(|| run(&args)) {
        Ok(Ok(code)) => ExitCode::from(code),
        Ok(Err(e)) => {
            eprintln!("{e}");
            ExitCode::from(e.exit_code())
        }
        Err(_) => {
            let e = LitmusError::Internal("unexpected panic".to_owned());
            eprintln!("{e}");
            ExitCode::from(e.exit_code())
        }
    }
}

fn run(args: &[String]) -> Result<u8, LitmusError> {
    #[cfg(debug_assertions)]
    if env::var_os("LITMUS_FORCE_PANIC").is_some() {
        panic!("LITMUS_FORCE_PANIC: triggered for exit-70 verification");
    }

    let dir = parse_args(args)?;
    let files = find_test_files(&dir);
    let result = analyze_files(&files);

    for error in &result.errors {
        eprintln!("{error}");
    }

    if result.issues.is_empty() {
        return Ok(EXIT_SUCCESS);
    }

    for issue in &result.issues {
        println!("{issue}");
    }

    Ok(EXIT_BLOCKING)
}

fn parse_args(args: &[String]) -> Result<PathBuf, LitmusError> {
    let rest = args.get(1..).unwrap_or(&[]);

    let mut positionals: Vec<&str> = Vec::new();
    for arg in rest {
        if arg.starts_with('-') {
            return Err(LitmusError::Usage(format!("unknown flag: {arg}")));
        }
        positionals.push(arg);
    }

    match positionals.as_slice() {
        [] => Ok(PathBuf::from(".")),
        [one] => Ok(PathBuf::from(one)),
        many => Err(LitmusError::Usage(format!(
            "expected at most 1 directory argument, got {}",
            many.len()
        ))),
    }
}
