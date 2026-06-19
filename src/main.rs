use litmus::rules::{Issue, Severity};
use litmus::{
    AnalysisResult, EXIT_BLOCKING, EXIT_SOFTWARE, EXIT_SUCCESS, EXIT_WARNING, FileError,
    FileErrorKind, LitmusError, analyze_files, find_test_files, json,
};
use std::env;
use std::io::{self, ErrorKind, Write};
use std::panic::{catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Output};
use std::thread;

const JSON_FLAG: &str = "--json";
const WORKER_FLAG: &str = "--worker-file";

struct Config {
    dir: PathBuf,
    json: bool,
    worker_file: Option<PathBuf>,
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
    if let Some(file) = &config.worker_file {
        return run_worker(file, config.json);
    }
    run_scan(&config.dir, config.json)
}

// A worker analyzes exactly one file in-process (on the large-stack analyzer
// thread + bracket guard) and emits its result, then exits. It never spawns
// children. The parent runs one worker per file, so a worker that aborts (stack
// overflow, oxc panic, OOM) takes down only its own process, never the batch.
//
// Output mirrors the pre-subprocess single-process format so the parent can
// relay it: text issues to stdout / errors to stderr; json as newline-framed
// `issues_frag\nerrors_frag` (both fragments are newline-free, so the frame is
// unambiguous) which the parent merges into one document.
fn run_worker(file: &Path, json: bool) -> Result<u8, LitmusError> {
    let files = [file.to_path_buf()];
    let result = run_analysis(&files);

    if json {
        let (issues, errors) = json::render_fragments(&result);
        if !emit(&format!("{issues}\n{errors}"))? {
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

// The parent never parses; it spawns one worker subprocess per file and
// aggregates their results, so no analysis crash can reach this process. The
// worker is `current_exe` re-invoked with `--worker-file`; if that path cannot
// be resolved there is no safe in-process fallback (running analysis here would
// reintroduce the crash this layer exists to prevent), so it is a hard error.
fn run_scan(dir: &Path, json: bool) -> Result<u8, LitmusError> {
    let files = find_test_files(dir);
    let exe = env::current_exe()
        .map_err(|e| LitmusError::Internal(format!("cannot locate litmus executable: {e}")))?;
    if json {
        run_scan_json(&exe, &files)
    } else {
        run_scan_text(&exe, &files)
    }
}

// Text mode: workers inherit stdout/stderr and write directly, so the parent
// only tracks the exit code (max wins). The 0/1/2 worker codes mirror
// EXIT_SUCCESS / EXIT_WARNING / EXIT_BLOCKING (they cannot be const match arms,
// so the literals carry that contract by comment). Any other code, or a death
// by signal, is a worker crash: the parent synthesizes a crash-class error on
// stderr and raises max to EXIT_SOFTWARE so the failure is loud, not a silent
// exit 0. A spawn failure is the same crash class (a worker that never launched
// is no more isolated than one that aborts) and must not `?`-abort the batch,
// otherwise one failed launch near a process limit discards every prior result.
fn run_scan_text(exe: &Path, files: &[PathBuf]) -> Result<u8, LitmusError> {
    let mut max = EXIT_SUCCESS;
    for file in files {
        let code = match spawn_status(exe, file) {
            Ok(status) => status.code(),
            Err(e) => {
                let err = FileError {
                    file: file.clone(),
                    kind: FileErrorKind::Crash,
                    message: spawn_failure_message(&e),
                };
                eprintln!("{err}");
                max = max.max(EXIT_SOFTWARE);
                continue;
            }
        };
        match code {
            Some(0) => {}
            Some(1) => max = max.max(EXIT_WARNING),
            Some(2) => max = max.max(EXIT_BLOCKING),
            _ => {
                let err = FileError {
                    file: file.clone(),
                    kind: FileErrorKind::Crash,
                    message: worker_abort_message(code),
                };
                eprintln!("{err}");
                max = max.max(EXIT_SOFTWARE);
            }
        }
    }
    Ok(max)
}

// Json mode: workers are captured (not inherited) so the parent can merge their
// newline-framed fragments into one `{"issues":[...],"errors":[...]}` document,
// keeping stderr empty. A crashed worker (signal death, unexpected code,
// malformed output, or a spawn failure) becomes a synthesized crash-class error
// fragment and raises max to EXIT_SOFTWARE, mirroring the text-mode loud failure
// so a crash is never a silent exit 0 in either mode.
fn run_scan_json(exe: &Path, files: &[PathBuf]) -> Result<u8, LitmusError> {
    let mut max = EXIT_SUCCESS;
    let mut issue_frags: Vec<String> = Vec::new();
    let mut error_frags: Vec<String> = Vec::new();

    for file in files {
        let output = match spawn_output(exe, file) {
            Ok(output) => output,
            Err(e) => {
                error_frags.push(crash_error_frag(file, &spawn_failure_message(&e)));
                max = max.max(EXIT_SOFTWARE);
                continue;
            }
        };
        let code = output.status.code();
        match code {
            Some(c @ 0..=2) => {
                if c == 1 {
                    max = max.max(EXIT_WARNING);
                } else if c == 2 {
                    max = max.max(EXIT_BLOCKING);
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some((issues, errors)) = stdout.split_once('\n') {
                    if !issues.is_empty() {
                        issue_frags.push(issues.to_owned());
                    }
                    if !errors.is_empty() {
                        error_frags.push(errors.to_owned());
                    }
                } else {
                    error_frags.push(crash_error_frag(
                        file,
                        "analysis aborted: worker produced malformed output",
                    ));
                    max = max.max(EXIT_SOFTWARE);
                }
            }
            _ => {
                error_frags.push(crash_error_frag(file, &worker_abort_message(code)));
                max = max.max(EXIT_SOFTWARE);
            }
        }
    }

    let doc = format!(
        "{{\"issues\":[{}],\"errors\":[{}]}}",
        issue_frags.join(","),
        error_frags.join(",")
    );
    if !emit(&doc)? {
        return Ok(EXIT_SUCCESS);
    }
    Ok(max)
}

// Spawn one worker per file. The debug-gated hook lets a test force a launch
// failure deterministically (the real failure mode — EAGAIN/ENOMEM near a
// process limit — cannot be provoked on demand), proving the parent isolates a
// spawn failure instead of aborting the batch. Release builds carry no env check.
fn spawn_status(exe: &Path, file: &Path) -> io::Result<ExitStatus> {
    #[cfg(debug_assertions)]
    if let Some(e) = forced_spawn_failure(file) {
        return Err(e);
    }
    Command::new(exe).arg(WORKER_FLAG).arg(file).status()
}

fn spawn_output(exe: &Path, file: &Path) -> io::Result<Output> {
    #[cfg(debug_assertions)]
    if let Some(e) = forced_spawn_failure(file) {
        return Err(e);
    }
    Command::new(exe)
        .arg(WORKER_FLAG)
        .arg(file)
        .arg(JSON_FLAG)
        .output()
}

#[cfg(debug_assertions)]
fn forced_spawn_failure(file: &Path) -> Option<io::Error> {
    let target = env::var_os("LITMUS_FORCE_SPAWN_FAIL")?;
    if file.to_string_lossy().contains(&*target.to_string_lossy()) {
        Some(io::Error::other("forced spawn failure (test hook)"))
    } else {
        None
    }
}

fn worker_abort_message(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("analysis aborted: worker exited with unexpected code {c}"),
        None => "analysis aborted: worker terminated by signal".to_owned(),
    }
}

fn spawn_failure_message(e: &io::Error) -> String {
    format!("analysis aborted: failed to spawn worker: {e}")
}

// Build the errors-array fragment for a worker that crashed, reusing the json
// escaping so a hostile file path cannot break the document.
fn crash_error_frag(file: &Path, message: &str) -> String {
    let result = AnalysisResult {
        issues: Vec::new(),
        errors: vec![FileError {
            file: file.to_path_buf(),
            kind: FileErrorKind::Crash,
            message: message.to_owned(),
        }],
    };
    json::render_fragments(&result).1
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
    let mut worker_file: Option<PathBuf> = None;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        if arg == JSON_FLAG {
            json = true;
        } else if arg == WORKER_FLAG {
            let value = iter.next().ok_or_else(|| {
                LitmusError::Usage(format!("{WORKER_FLAG} requires a path argument"))
            })?;
            worker_file = Some(PathBuf::from(value));
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

    Ok(Config {
        dir,
        json,
        worker_file,
    })
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
