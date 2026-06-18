use litmus::rules::{Issue, Severity};
use litmus::{
    EXIT_BLOCKING, EXIT_SUCCESS, EXIT_WARNING, LitmusError, analyze_files, find_test_files, json,
};
use std::env;
use std::io::{self, ErrorKind, Write};
use std::panic::catch_unwind;
use std::path::PathBuf;
use std::process::ExitCode;

const JSON_FLAG: &str = "--json";

struct Config {
    dir: PathBuf,
    json: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    // The `--json` flag governs error formatting too, so detect it before
    // parse_args can fail on an unrelated argument.
    let json_mode = args.iter().any(|a| a == JSON_FLAG);

    match catch_unwind(|| run(&args)) {
        Ok(Ok(code)) => ExitCode::from(code),
        Ok(Err(e)) => {
            print_error(&e, json_mode);
            ExitCode::from(e.exit_code())
        }
        Err(_) => {
            let e = LitmusError::Internal("unexpected panic".to_owned());
            print_error(&e, json_mode);
            ExitCode::from(e.exit_code())
        }
    }
}

fn print_error(e: &LitmusError, json_mode: bool) {
    if json_mode {
        eprintln!("{}", json::render_error(e));
    } else {
        eprintln!("{e}");
    }
}

fn run(args: &[String]) -> Result<u8, LitmusError> {
    #[cfg(debug_assertions)]
    if env::var_os("LITMUS_FORCE_PANIC").is_some() {
        panic!("LITMUS_FORCE_PANIC: triggered for exit-70 verification");
    }

    let config = parse_args(args)?;
    let files = find_test_files(&config.dir);
    let result = analyze_files(&files);

    if config.json {
        if !emit(&json::render_result(&result))? {
            return Ok(EXIT_SUCCESS);
        }
        return Ok(select_exit_code(&result.issues));
    }

    for error in &result.errors {
        eprintln!("{error}");
    }

    if result.issues.is_empty() {
        return Ok(EXIT_SUCCESS);
    }

    let mut buf = String::new();
    for issue in &result.issues {
        buf.push_str(&issue.to_string());
        buf.push('\n');
    }
    if !emit(&buf)? {
        return Ok(EXIT_SUCCESS);
    }

    Ok(select_exit_code(&result.issues))
}

// Write to stdout in one shot. A closed reader (`litmus | head`) surfaces as
// BrokenPipe rather than a panic, because Rust leaves SIGPIPE at SIG_IGN.
// Returns Ok(false) on BrokenPipe so the caller stops cleanly with exit 0
// (the reader already left), and Ok(true) once fully written.
fn emit(s: &str) -> Result<bool, LitmusError> {
    let mut out = io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(LitmusError::Internal(format!("stdout write failed: {e}"))),
    }
}

// No issues → success; a single blocking issue forces exit 2; otherwise
// warning-only issues yield exit 1. The empty case is encoded here (not just at
// the call sites) so the exit code stays correct for every caller.
fn select_exit_code(issues: &[Issue]) -> u8 {
    if issues.is_empty() {
        EXIT_SUCCESS
    } else if issues
        .iter()
        .any(|issue| issue.severity() == Severity::Blocking)
    {
        EXIT_BLOCKING
    } else {
        EXIT_WARNING
    }
}

fn parse_args(args: &[String]) -> Result<Config, LitmusError> {
    let rest = args.get(1..).unwrap_or(&[]);

    let mut positionals: Vec<&str> = Vec::new();
    let mut json = false;
    for arg in rest {
        if arg == "--json" {
            json = true;
        } else if arg.starts_with('-') {
            return Err(LitmusError::Usage(format!("unknown flag: {arg}")));
        } else {
            positionals.push(arg);
        }
    }

    let dir = match positionals.as_slice() {
        [] => PathBuf::from("."),
        [one] => PathBuf::from(one),
        many => {
            return Err(LitmusError::Usage(format!(
                "expected at most 1 directory argument, got {}",
                many.len()
            )));
        }
    };

    Ok(Config { dir, json })
}

#[cfg(test)]
mod tests {
    use super::{EXIT_BLOCKING, EXIT_WARNING, Issue, parse_args, select_exit_code};
    use std::path::PathBuf;

    fn issue(rule: &'static str) -> Issue {
        Issue {
            rule,
            file: PathBuf::from("test.ts"),
            line: 1,
            test_name: "test case".to_owned(),
            detail: String::new(),
        }
    }

    // T-407: only warning-level issues → exit 1
    #[test]
    fn warning_only_exits_1() {
        let issues = vec![issue("dummy-data")];
        assert_eq!(select_exit_code(&issues), EXIT_WARNING);
    }

    // T-408: any blocking issue overrides warning → exit 2
    #[test]
    fn blocking_overrides_warning_exits_2() {
        let issues = vec![issue("dummy-data"), issue("weak-assertion")];
        assert_eq!(select_exit_code(&issues), EXIT_BLOCKING);
    }

    // T-J07: --json sets the flag and still resolves the directory positional
    #[test]
    fn parse_args_accepts_json_flag_with_dir() {
        let args = vec!["litmus".to_owned(), "--json".to_owned(), "src".to_owned()];
        let config = parse_args(&args).unwrap();
        assert!(config.json);
        assert_eq!(config.dir, PathBuf::from("src"));
    }

    // T-J08: --json alone defaults the directory to "."
    #[test]
    fn parse_args_json_only_defaults_dir() {
        let args = vec!["litmus".to_owned(), "--json".to_owned()];
        let config = parse_args(&args).unwrap();
        assert!(config.json);
        assert_eq!(config.dir, PathBuf::from("."));
    }
}
