use litmus::rules::{Issue, Severity};
use litmus::{
    AnalysisResult, EXIT_BLOCKING, EXIT_SUCCESS, EXIT_WARNING, LitmusError, analyze_files,
    find_test_files, json,
};
use std::env;
use std::io::{self, ErrorKind, Write};
use std::panic::{catch_unwind, resume_unwind};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

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
    let result = run_analysis(&files);

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

// oxc's recursive-descent parser recurses on right-associative forms (ternary
// alternate spine, assignment `=`, exponent `**`, prefix-unary) with no brackets
// to bound them, so the `max_bracket_depth` guard cannot catch them and a deep
// chain overflows the native stack (SIGABRT, uncatchable by `catch_unwind`).
// Each right-associative frame costs ~1KB, so a 256 MiB stack lifts their
// overflow floor to ~250k levels (measured: ~200k parses, ~300k aborts) — ~4
// orders of magnitude above any human-authored or transpiled test source. The
// size is a lazy virtual reservation (guard-paged), not a physical commit.
const ANALYZER_STACK_SIZE: usize = 256 * 1024 * 1024;

fn run_analysis(files: &[PathBuf]) -> AnalysisResult {
    run_analysis_with_stack(files, ANALYZER_STACK_SIZE)
}

// Spawning can fail when the environment caps address space below the
// reservation (`ulimit -v`, a cgroup memory limit). Falling back to the main
// thread degrades to the pre-thread behavior instead of aborting with exit 70,
// which matters because litmus runs on every AI edit via a hook. A panic inside
// the analyzer thread is re-raised here via `resume_unwind`, which forwards the
// original payload (the child thread's hook already printed it) without a second
// "analyzer thread panicked" line; main's `catch_unwind` then maps it to exit 70.
fn run_analysis_with_stack(files: &[PathBuf], stack_size: usize) -> AnalysisResult {
    let builder = thread::Builder::new().stack_size(stack_size);
    thread::scope(
        |scope| match builder.spawn_scoped(scope, || analyze_files(files)) {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|payload| resume_unwind(payload)),
            Err(_) => analyze_files(files),
        },
    )
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
    use super::{
        EXIT_BLOCKING, EXIT_WARNING, Issue, analyze_files, find_test_files, parse_args,
        run_analysis_with_stack, select_exit_code,
    };
    use std::fs;
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

    // T-057: a stack size the OS cannot reserve makes spawn_scoped fail, so
    // run_analysis_with_stack must fall back to main-thread analysis rather than
    // panic. usize::MAX returns Err on the distributed Unix targets (the trigger
    // is platform-dependent; where it instead spawns, the spawned arm is
    // exercised). Either way the result must match a direct analyze_files call,
    // so a broken fallback (e.g. returning an empty/default result) is caught.
    #[test]
    fn run_analysis_with_unspawnable_stack_matches_direct_analysis() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.test.ts"),
            r#"test("weak", () => { expect(x).toBeTruthy() })"#,
        )
        .unwrap();
        let files = find_test_files(dir.path());
        let direct = analyze_files(&files);
        assert!(!direct.issues.is_empty(), "fixture must produce issues");

        let result = run_analysis_with_stack(&files, usize::MAX);

        assert_eq!(
            result.issues.len(),
            direct.issues.len(),
            "fallback must return the same analysis as a direct call"
        );
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
