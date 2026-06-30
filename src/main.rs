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

// The worker/parent split implements the ADR-0066 fault-isolation contract,
// which rests on three guarantees kept here and in the parent loops:
//   1. one worker per file, single layer — a worker analyzes exactly one file
//      in-process and never spawns children, so a worker that aborts (stack
//      overflow, oxc panic, OOM) takes down only its own process, never the batch.
//   2. a spawn failure does not abort the batch (it is a crash-class error and
//      `continue`s; see run_scan_text / run_scan_json).
//   3. the worker->parent wire format is newline-framed `issues_frag\nerrors_frag`,
//      unambiguous because `escape` strips every control char from both fragments.
//
// A worker analyzes exactly one file in-process (on the large-stack analyzer
// thread + bracket guard) and emits its result, then exits.
//
// Output mirrors the pre-subprocess single-process format so the parent can
// relay it: text issues to stdout / errors to stderr; json as the newline-framed
// fragments above, which the parent merges into one document.
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

// litmus's input contract is a directory path. A nonexistent path or a file
// (e.g. a single `.test.ts` passed directly) globs to zero matches and would
// otherwise exit 0 "clean", masking bad input as "no findings" — a silent false
// negative for a CI / hook caller. Reject it as a usage error (exit 64) so the
// caller can tell "scanned, 0 violations" from "scanned nothing, input was
// wrong". An existing empty directory is valid input and still yields exit 0.
fn validate_scan_dir(dir: &Path) -> Result<(), LitmusError> {
    if !dir.exists() {
        return Err(LitmusError::Usage(format!(
            "path does not exist: {}",
            dir.display()
        )));
    }
    if !dir.is_dir() {
        return Err(LitmusError::Usage(format!(
            "not a directory: {}",
            dir.display()
        )));
    }
    Ok(())
}

// The parent never parses; it spawns one worker subprocess per file and
// aggregates their results, so no analysis crash can reach this process. The
// worker is `current_exe` re-invoked with `--worker-file`; if that path cannot
// be resolved there is no safe in-process fallback (running analysis here would
// reintroduce the crash this layer exists to prevent), so it is a hard error.
fn run_scan(dir: &Path, json: bool) -> Result<u8, LitmusError> {
    validate_scan_dir(dir)?;
    let files = find_test_files(dir);
    let exe = env::current_exe()
        .map_err(|e| LitmusError::Internal(format!("cannot locate litmus executable: {e}")))?;
    if json {
        run_scan_json(&exe, &files)
    } else {
        run_scan_text(&exe, &files)
    }
}

// A worker subprocess exits with a code that mirrors the EXIT_* contract; the
// parent maps it back onto that contract here. Matching the EXIT_* constants
// (not 0/1/2 literals) keeps the mapping correct if a constant's value changes,
// so the aggregation precedence never silently breaks when lib.rs is edited.
enum WorkerOutcome {
    // The worker exited cleanly with a contract code (success / warning / blocking).
    Code(u8),
    // A death by signal (code == None) or an unrecognized code: a worker crash.
    Crash,
}

fn classify_worker_code(code: Option<i32>) -> WorkerOutcome {
    match code {
        Some(c) if c == i32::from(EXIT_SUCCESS) => WorkerOutcome::Code(EXIT_SUCCESS),
        Some(c) if c == i32::from(EXIT_WARNING) => WorkerOutcome::Code(EXIT_WARNING),
        Some(c) if c == i32::from(EXIT_BLOCKING) => WorkerOutcome::Code(EXIT_BLOCKING),
        _ => WorkerOutcome::Crash,
    }
}

// Text mode: workers inherit stdout/stderr and write directly, so the parent
// only tracks the exit code. Severity aggregates by `max` over the EXIT_*
// constants, whose values encode the precedence crash (70) > blocking (2) >
// warning (1) > clean (0): the highest-severity worker decides the batch exit
// code. Any unrecognized code, or a death by signal, is a worker crash: the
// parent synthesizes a crash-class error on stderr and raises max to
// EXIT_SOFTWARE so the failure is loud, not a silent
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
        match classify_worker_code(code) {
            WorkerOutcome::Code(c) => max = max.max(c),
            WorkerOutcome::Crash => {
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
        match classify_worker_code(code) {
            WorkerOutcome::Code(c) => {
                max = max.max(c);
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
            WorkerOutcome::Crash => {
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
//
// This stack only bounds the right-associative shapes. Bracket-nesting recursion
// is rejected pre-parse by parse.rs `BRACKET_DEPTH_LIMIT`, which is sized against
// this stack's bracket overflow floor (~86k levels at ~3KB/frame): shrinking
// ANALYZER_STACK_SIZE lowers that floor, so the two constants are coupled and
// must move together.
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
        EXIT_BLOCKING, EXIT_SOFTWARE, EXIT_SUCCESS, EXIT_WARNING, Issue, WorkerOutcome,
        analyze_files, classify_worker_code, find_test_files, parse_args, run_analysis_with_stack,
        select_exit_code, validate_scan_dir,
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

    // #89 A2: find_test_files discovers `.spec.` and every supported JS/TS/ESM
    // extension, not just `.test.ts(x)`. A JS/spec project previously matched
    // zero files → litmus silently exited 0 (fail-as-success). The fixture
    // covers all eight TEST_EXTENSIONS so a dropped const entry is caught.
    //
    // Three exclusion classes are each represented so a broken guard is caught:
    // (1) `helper.ts` lacks the `.test.`/`.spec.` infix → the glob never yields
    // it; (2) `node_modules/dep.test.js` → is_excluded; (3) the snapshot and
    // `.test.json` files match the widened `**/*.test.*` glob and sit outside
    // EXCLUDED_DIRS (`__snapshots__` is not excluded), so only has_test_extension
    // keeps them out of the parser. Without that filter snapshot/JSON noise would
    // reach analyze_files.
    #[test]
    fn find_test_files_covers_spec_and_js_extensions() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        for name in [
            "a.test.ts",
            "b.spec.ts",
            "c.test.js",
            "d.spec.jsx",
            "e.test.mjs",
            "f.test.cts",
            "g.test.tsx",
            "h.spec.cjs",
            "i.test.mts",
        ] {
            fs::write(root.join(name), "test(\"t\", () => {})").unwrap();
        }
        // Excluded: no test/spec infix, and inside node_modules.
        fs::write(root.join("helper.ts"), "export const x = 1").unwrap();
        let nm = root.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("dep.test.js"), "test(\"t\", () => {})").unwrap();
        // Excluded by extension: matches the glob but is not a test source.
        let snap = root.join("__snapshots__");
        fs::create_dir_all(&snap).unwrap();
        fs::write(snap.join("comp.test.ts.snap"), "exports[`x`] = `y`;").unwrap();
        fs::write(root.join("fixture.test.json"), "{}").unwrap();

        let mut found: Vec<String> = find_test_files(root)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                "a.test.ts",
                "b.spec.ts",
                "c.test.js",
                "d.spec.jsx",
                "e.test.mjs",
                "f.test.cts",
                "g.test.tsx",
                "h.spec.cjs",
                "i.test.mts",
            ]
        );
    }

    // #89 A2: a file matching both globs (`.test.` and `.spec.` infix) is
    // analyzed once, not duplicated.
    #[test]
    fn find_test_files_dedups_test_and_spec_infix() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("a.test.spec.ts"), "test(\"t\", () => {})").unwrap();
        assert_eq!(find_test_files(dir.path()).len(), 1);
    }

    // T-029a: a nonexistent path is a usage error, not a clean exit 0; the
    // message names the offending path so the caller can correct it.
    #[test]
    fn validate_scan_dir_rejects_nonexistent_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("does_not_exist_xyz");
        assert!(
            matches!(validate_scan_dir(&missing), Err(litmus::LitmusError::Usage(m)) if m.contains("does not exist")),
            "expected a usage error naming the missing path"
        );
    }

    // T-029b: a file path (not a directory) is a usage error; litmus's input
    // contract is a directory, and a file globs to zero matches → false exit 0.
    #[test]
    fn validate_scan_dir_rejects_file_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("bad.test.ts");
        fs::write(&file, "test(\"t\", () => {})").unwrap();
        assert!(
            matches!(validate_scan_dir(&file), Err(litmus::LitmusError::Usage(m)) if m.contains("not a directory")),
            "expected a usage error labeling the file as not a directory"
        );
    }

    // T-029c: an existing directory is valid input even when empty; "0 test
    // files" stays a clean exit 0, distinct from bad input.
    #[test]
    fn validate_scan_dir_accepts_existing_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(validate_scan_dir(dir.path()).is_ok());
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

    // T-060: --worker-file with no following value is a usage error, not a
    // silent default; the message names the flag that needs an argument.
    #[test]
    fn parse_args_worker_file_without_value_is_usage_error() {
        let args = vec!["litmus".to_owned(), "--worker-file".to_owned()];
        assert!(
            matches!(parse_args(&args), Err(litmus::LitmusError::Usage(m)) if m.contains("--worker-file")),
            "expected a usage error naming --worker-file when no value follows"
        );
    }

    // T-058: the parent aggregates worker exit codes by `max` over the EXIT_*
    // constants, whose ordering encodes crash > blocking > warning > clean.
    // classify_worker_code maps each contract code through those constants (not
    // 0/1/2 literals), so a change to any constant value re-derives the expected
    // mapping here and a stale literal would be caught instead of silently
    // breaking the precedence.
    #[test]
    fn worker_code_classifier_tracks_exit_constants() {
        const {
            assert!(EXIT_SUCCESS < EXIT_WARNING);
            assert!(EXIT_WARNING < EXIT_BLOCKING);
            assert!(EXIT_BLOCKING < EXIT_SOFTWARE);
        }
        for code in [EXIT_SUCCESS, EXIT_WARNING, EXIT_BLOCKING] {
            match classify_worker_code(Some(i32::from(code))) {
                WorkerOutcome::Code(c) => assert_eq!(c, code),
                WorkerOutcome::Crash => panic!("contract code {code} misclassified as crash"),
            }
        }
        // signal death and an internal-error worker (exit 70) are both crashes
        assert!(matches!(classify_worker_code(None), WorkerOutcome::Crash));
        assert!(matches!(
            classify_worker_code(Some(i32::from(EXIT_SOFTWARE))),
            WorkerOutcome::Crash
        ));
    }
}
