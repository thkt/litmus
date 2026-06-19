use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn litmus_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_litmus"))
}

fn litmus(dir: &Path) -> Output {
    litmus_cmd()
        .arg(dir)
        .output()
        .expect("failed to run litmus")
}

// T-015: issues present → exit 2 + stdout has file path and line number
#[test]
fn exit_2_with_issues() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("weak.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("weak-assertion"), "stdout: {stdout}");
    assert!(stdout.contains("weak.test.ts:1"), "stdout: {stdout}");
    assert!(stdout.contains("weak only"), "stdout: {stdout}");
}

// T-015 variant: mock overuse also reported
#[test]
fn exit_2_mock_overuse() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mocks.test.ts"),
        r#"test("too many mocks", () => {
    const a = vi.fn()
    const b = vi.fn()
    const c = vi.fn()
    expect(result).toBe(1)
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("mock-overuse"), "stdout: {stdout}");
    assert!(
        stdout.contains("mocks: 3, assertions: 1"),
        "stdout: {stdout}"
    );
}

// T-016: no issues → exit 0
#[test]
fn exit_0_no_issues() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("good.test.ts"),
        r#"test("returns correct user data", () => {
    expect(result).toBe(42)
    expect(name).toEqual("hello")
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8(output.stdout).unwrap().is_empty());
}

// T-017: no test files → exit 0
#[test]
fn exit_0_no_test_files() {
    let dir = TempDir::new().unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(0));
}

// T-018: parse error file skipped, others still processed
#[test]
fn parse_error_skipped_others_processed() {
    let dir = TempDir::new().unwrap();

    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    fs::write(dir.path().join("broken.test.ts"), "@@@ not javascript $$$").unwrap();

    let output = litmus(dir.path());

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("broken.test.ts"),
        "stderr should warn about broken file: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("valid.test.ts"),
        "stdout should still report valid file issues: {stdout}"
    );
}

// T-025 (issue #25): a file nested deeper than litmus parses would overflow
// oxc's stack and abort the process with SIGABRT (exit code via signal, status
// .code() == None). The pre-parse depth guard turns it into a per-file parse
// error reported on stderr, so the process exits cleanly and the sibling valid
// file is still analyzed (exit 2 from its weak assertion, never a signal abort).
#[test]
fn deeply_nested_file_skipped_not_aborted() {
    let dir = TempDir::new().unwrap();

    // 4000 is well past the measured release overflow floor (~2700) for
    // expression bracket nesting, so before the guard this file aborted the
    // process; it is also well past the guard's limit (500), so after the guard
    // it is rejected without ever parsing.
    let n = 4000;
    let deep = format!("const y = {}0{};", "[".repeat(n), "]".repeat(n));
    fs::write(dir.path().join("deep.test.ts"), deep).unwrap();

    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(
        output.status.code(),
        Some(2),
        "must exit cleanly from the valid file, not abort on SIGABRT"
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("deep.test.ts") && stderr.contains("parse error"),
        "stderr should report the deep file as a parse error: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("valid.test.ts"),
        "sibling valid file must still be analyzed: {stdout}"
    );
}

// T-056 (issue #56): a deep right-associative ternary recurses in oxc with no
// brackets to count, so the #25 depth guard (which scans only `{[(`) lets it
// through. On the 8MB main stack this overflowed and aborted with SIGABRT
// (status.code() == None); running analysis on the 256 MiB analyzer thread
// raises the floor past this depth, so the process exits cleanly and the
// sibling valid file is still analyzed (exit 2 from its weak assertion).
#[test]
fn deep_right_recursive_ternary_rescued_not_aborted() {
    let dir = TempDir::new().unwrap();

    // 50000 is well past the ~12000 main-stack ternary overflow floor but well
    // under the ~250000 in-thread floor. The chain `c?c?…?x:y:y` contains no
    // bracket byte, so max_bracket_depth == 0 and the guard cannot reject it.
    let n = 50000;
    let ternary = format!("const z = {}x{};", "c?".repeat(n), ":y".repeat(n));
    fs::write(dir.path().join("deep.test.ts"), ternary).unwrap();

    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(
        output.status.code(),
        Some(2),
        "must exit cleanly from the valid file, not abort on SIGABRT"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("valid.test.ts"),
        "sibling valid file must still be analyzed: {stdout}"
    );
}

// RC-002: .test.tsx files detected and analyzed
#[test]
fn tsx_files_detected() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("comp.test.tsx"),
        r#"test("tsx weak", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("comp.test.tsx"), "stdout: {stdout}");
}

// TC-007: read error (directory where file expected) — skipped, others processed
#[test]
fn read_error_skipped_others_processed() {
    let dir = TempDir::new().unwrap();

    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    fs::create_dir(dir.path().join("unreadable.test.ts")).unwrap();

    let output = litmus(dir.path());

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unreadable.test.ts"),
        "stderr should warn about unreadable file: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("valid.test.ts"),
        "valid file should still be processed: {stdout}"
    );
}

// node_modules excluded from scanning
#[test]
fn excludes_node_modules() {
    let dir = TempDir::new().unwrap();

    let nm = dir.path().join("node_modules/zod/src");
    fs::create_dir_all(&nm).unwrap();
    fs::write(
        nm.join("base.test.ts"),
        r#"test("weak", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "node_modules should be excluded"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.is_empty(), "no output expected: {stdout}");
}

// T-040: tautological + mock-only detection via CLI
#[test]
fn detects_tautological_and_mock_only() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("behavior.test.ts"),
        r#"
test("tautological", () => {
    expect(true).toBe(true)
})
test("mock only", () => {
    expect(mockFn).toHaveBeenCalledWith("/api")
    expect(mockFn).toHaveBeenCalledTimes(1)
})
"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("tautological"), "stdout: {stdout}");
    assert!(stdout.contains("mock-only"), "stdout: {stdout}");
}

// T-052: short test name → exit 2 with test-name-quality
#[test]
fn test_name_quality_short_name_detected() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("naming.test.ts"),
        r#"test("should work", () => {
    expect(result).toBe(42)
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("test-name-quality"), "stdout: {stdout}");
    assert!(stdout.contains("should work"), "stdout: {stdout}");
    assert!(stdout.contains("words: 2"), "stdout: {stdout}");
}

// T-053: 4-word test name → exit 0
#[test]
fn test_name_quality_good_name_passes() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("good_name.test.ts"),
        r#"test("returns user by id", () => {
    expect(result).toBe(42)
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(0));
}

// T-301: empty body → empty-test
#[test]
fn detects_empty_test() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("empty.test.ts"),
        r#"test("does nothing", () => {})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("empty-test"), "stdout: {stdout}");
}

// T-302: test.skip → skipped-test
#[test]
fn detects_skipped_test() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("skip.test.ts"),
        r#"test.skip("skipped test case", () => {
    expect(result).toBe(42)
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("skipped-test"), "stdout: {stdout}");
}

// T-303: try-catch swallow → catch-swallow
#[test]
fn detects_catch_swallow() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("swallow.test.ts"),
        r#"test("swallows errors silently", () => {
    try {
        riskyOperation()
        expect(result).toBe(42)
    } catch (e) {}
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("catch-swallow"), "stdout: {stdout}");
}

// T-303b: rethrow nested in an if is not a catch-swallow false positive (#27)
#[test]
fn nested_rethrow_is_not_catch_swallow() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("rethrow.test.ts"),
        r#"test("rethrows error when condition holds", () => {
    try { risky() } catch (e) { if (e) { throw e } }
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("catch-swallow"), "stdout: {stdout}");
}

// T-I1: try assertion swallowed by catch assertion → catch-masks-assertion, exit 2
#[test]
fn detects_catch_masks_assertion() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("masks.test.ts"),
        r#"test("masks the real assertion failure", () => {
    try {
        expect(actual).toBe(expected)
    } catch (e) {
        expect(e).toBeDefined()
    }
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("catch-masks-assertion"), "stdout: {stdout}");
}

// T-304: all assertions in if → conditional-assertion
#[test]
fn detects_conditional_assertion() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("conditional.test.ts"),
        r#"test("only asserts conditionally", () => {
    const result = getResult()
    if (result) {
        expect(result.name).toBe("foo")
    }
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("conditional-assertion"), "stdout: {stdout}");
}

// T-305: all assertions in catch → catch-only-assertion
#[test]
fn detects_catch_only_assertion() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("catchonly.test.ts"),
        r#"test("only asserts on error", () => {
    try {
        dangerousOp()
    } catch (e) {
        expect(e.message).toBe("fail")
    }
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("catch-only-assertion"), "stdout: {stdout}");
}

// T-306: empty body → empty-test only (no weak-assertion)
#[test]
fn empty_test_suppresses_weak_assertion() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("empty_only.test.ts"),
        r#"test("empty suppresses weak", () => {})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("empty-test"),
        "should have empty-test: {stdout}"
    );
    assert!(
        !stdout.contains("weak-assertion"),
        "should NOT have weak-assertion: {stdout}"
    );
}

// TC-005: mixed TopLevel + if assertions → no conditional-assertion
#[test]
fn mixed_conditional_no_issue() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("mixed.test.ts"),
        r#"test("mixed assertions are fine", () => {
    expect(base).toBe(1)
    if (condition) {
        expect(extra).toBe(2)
    }
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(0));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("conditional-assertion"),
        "should NOT have conditional-assertion: {stdout}"
    );
}

// TC-008: comment-only body → empty-test
#[test]
fn comment_only_body_is_empty() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("comment.test.ts"),
        r#"test("placeholder with comment", () => {
    // TODO: implement this test
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("empty-test"),
        "should have empty-test: {stdout}"
    );
}

// T-410: usage error - 2+ positional args → exit 64
#[test]
fn bad_usage_too_many_positional_args() {
    let dir1 = TempDir::new().unwrap();
    let dir2 = TempDir::new().unwrap();

    let output = litmus_cmd()
        .arg(dir1.path())
        .arg(dir2.path())
        .output()
        .expect("failed to run litmus");

    assert_eq!(output.status.code(), Some(64));

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("usage error"),
        "stderr should label usage error: {stderr}"
    );
}

// T-029d: a nonexistent path is a usage error (exit 64), not a silent clean
// exit 0; stderr names the missing path so a CI / hook caller can correct it.
#[test]
fn nonexistent_path_is_usage_error() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("does_not_exist_xyz");

    let output = litmus_cmd()
        .arg(&missing)
        .output()
        .expect("failed to run litmus");

    assert_eq!(output.status.code(), Some(64));

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("does not exist"),
        "stderr should report the missing path: {stderr}"
    );
}

// T-029e: a file path (litmus expects a directory) is a usage error (exit 64),
// not a clean exit 0 that masks the file's weak assertions as "no findings".
#[test]
fn file_path_is_usage_error() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("bad.test.ts");
    fs::write(&file, r#"test("t", () => { expect(x).toBeTruthy() })"#).unwrap();

    let output = litmus_cmd()
        .arg(&file)
        .output()
        .expect("failed to run litmus");

    assert_eq!(output.status.code(), Some(64));

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("not a directory"),
        "stderr should label the file as not a directory: {stderr}"
    );
}

// T-411: usage error - unknown flag → exit 64
#[test]
fn bad_usage_unknown_flag() {
    let output = litmus_cmd()
        .arg("--unknown")
        .output()
        .expect("failed to run litmus");

    assert_eq!(output.status.code(), Some(64));

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unknown flag"),
        "stderr should mention unknown flag: {stderr}"
    );
}

// T-412: panic inside run() → catch_unwind maps to exit 70 (EX_SOFTWARE).
// The LITMUS_FORCE_PANIC env var path is gated by `#[cfg(debug_assertions)]`
// in `main.rs`, so this test is likewise gated — under `cargo test --release`
// the env var is ignored and the assertion would falsely fail.
#[cfg(debug_assertions)]
#[test]
fn internal_error_panic_returns_70() {
    let dir = TempDir::new().unwrap();

    let output = litmus_cmd()
        .arg(dir.path())
        .env("LITMUS_FORCE_PANIC", "1")
        .output()
        .expect("failed to run litmus");

    assert_eq!(
        output.status.code(),
        Some(70),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("litmus: internal error"),
        "stderr should include the litmus internal-error prefix: {stderr}"
    );
}

// T-413: ADR-0066 Group 3 — standalone CLI and hook-spawned invocations must
// produce identical exit codes. Spawn via `sh -c` to simulate the gates/hook
// embedding path.
#[test]
fn spawned_via_sh_wrapper_matches_direct_exit_code() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("weak.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let direct = litmus(dir.path());
    assert_eq!(direct.status.code(), Some(2));

    let wrapped = Command::new("sh")
        .arg("-c")
        .arg(r#""$0" "$1""#)
        .arg(env!("CARGO_BIN_EXE_litmus"))
        .arg(dir.path())
        .output()
        .expect("failed to spawn via sh");

    assert_eq!(
        wrapped.status.code(),
        direct.status.code(),
        "wrapped exit code should match direct invocation"
    );
}

// T-247: arrange-only body (local data + strong assertion, no SUT call) →
// missing-act at warning severity, exit 1.
#[test]
fn exit_1_missing_act() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("noact.test.ts"),
        r#"test("computes the discounted total", () => {
    const total = 42
    expect(total).toBe(42)
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("missing-act"), "stdout: {stdout}");
    assert!(stdout.contains("noact.test.ts:1"), "stdout: {stdout}");
}

// T-421: an external snapshot assertion (toMatchSnapshot) → snapshot-external at
// warning severity, exit 1. The render() Act call keeps missing-act silent.
#[test]
fn exit_1_snapshot_external() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("snapshot.test.ts"),
        r#"test("renders the user card markup", () => {
    const html = render(user)
    expect(html).toMatchSnapshot()
})"#,
    )
    .unwrap();

    let output = litmus(dir.path());
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("snapshot-external"), "stdout: {stdout}");
    assert!(stdout.contains("snapshot.test.ts:3"), "stdout: {stdout}");
}

// T-J09: --json with issues → stdout is a JSON document carrying the rule and
// severity; exit code is unchanged (blocking → 2).
#[test]
fn json_mode_emits_issues_document() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("weak.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .output()
        .expect("failed to run litmus");
    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with(r#"{"issues":["#), "stdout: {stdout}");
    assert!(
        stdout.contains(r#""rule":"weak-assertion""#),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(r#""severity":"blocking""#),
        "stdout: {stdout}"
    );
    assert!(stdout.contains(r#""errors":[]"#), "stdout: {stdout}");
    // stderr stays empty: the JSON document is the sole output stream.
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
}

// T-J10: --json with no issues → empty arrays, exit 0.
#[test]
fn json_mode_clean_is_empty_arrays() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("ok.test.ts"),
        r#"test("returns the sum of two positive integers", () => {
    const total = add(2, 3)
    expect(total).toBe(5)
})"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .output()
        .expect("failed to run litmus");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim_end(), r#"{"issues":[],"errors":[]}"#);
}

// T-J11: --json with a parse error surfaces the error in the document, not on
// stderr, while non-erroring files still report issues.
#[test]
fn json_mode_carries_file_errors() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("broken.test.ts"), "test(\"x\", () => {").unwrap();

    let output = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .output()
        .expect("failed to run litmus");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#""kind":"parse""#), "stdout: {stdout}");
    assert!(stdout.contains("broken.test.ts"), "stdout: {stdout}");
    assert!(
        output.stderr.is_empty(),
        "stderr should be empty in json mode"
    );
}

// T-J12: --json on a usage error → error JSON on stderr with next_step +
// candidates; exit 64.
#[test]
fn json_mode_usage_error_has_next_step() {
    let output = litmus_cmd()
        .arg("--json")
        .arg("--bogus")
        .output()
        .expect("failed to run litmus");
    assert_eq!(output.status.code(), Some(64));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(r#""error":"usage""#), "stderr: {stderr}");
    assert!(stderr.contains(r#""next_step":"#), "stderr: {stderr}");
    assert!(
        stderr.contains(r#""candidates":["--json"]"#),
        "stderr: {stderr}"
    );
    assert!(output.stdout.is_empty(), "stdout should be empty on error");
}

// T-J13: a reader that closes early (BrokenPipe) does not crash litmus into the
// internal-error exit (70); it stops cleanly at 0. Output is sized past the
// pipe buffer so the writer is still writing when the reader closes.
#[test]
fn broken_pipe_does_not_panic() {
    use std::io::Read;
    use std::process::Stdio;

    let dir = TempDir::new().unwrap();
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!(
            "test(\"weak {i}\", () => {{ expect(x).toBeTruthy() }})\n"
        ));
    }
    fs::write(dir.path().join("many.test.ts"), body).unwrap();

    let mut child = litmus_cmd()
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn litmus");

    {
        let mut out = child.stdout.take().unwrap();
        let mut buf = [0u8; 64];
        let _ = out.read(&mut buf);
        // Dropping `out` closes the read end; further writes hit BrokenPipe.
    }

    let status = child.wait().expect("failed to wait on litmus");
    assert_eq!(
        status.code(),
        Some(0),
        "broken pipe must stop cleanly, not exit 70"
    );
}

// T-J16: --json with only warning-level issues → exit 1, mirroring text mode.
// Pins the json warning branch distinctly from the blocking (exit 2) path.
#[test]
fn json_mode_warning_only_exits_1() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("noact.test.ts"),
        r#"test("computes the discounted total", () => {
    const total = 42
    expect(total).toBe(42)
})"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .output()
        .expect("failed to run litmus");
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(r#""rule":"missing-act""#),
        "stdout: {stdout}"
    );
    assert!(stdout.contains(r#""errors":[]"#), "stdout: {stdout}");
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
}

// T-J17: a reader closing early in json mode hits BrokenPipe while the parent
// writes the merged document; like text mode it stops cleanly at 0, not 70. One
// file with many weak tests makes the merged json doc outgrow the pipe buffer.
#[test]
fn json_mode_broken_pipe_does_not_panic() {
    use std::io::Read;
    use std::process::Stdio;

    let dir = TempDir::new().unwrap();
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!(
            "test(\"weak {i}\", () => {{ expect(x).toBeTruthy() }})\n"
        ));
    }
    fs::write(dir.path().join("many.test.ts"), body).unwrap();

    let mut child = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn litmus");

    {
        let mut out = child.stdout.take().unwrap();
        let mut buf = [0u8; 64];
        let _ = out.read(&mut buf);
        // Dropping `out` closes the read end; further writes hit BrokenPipe.
    }

    let status = child.wait().expect("failed to wait on litmus");
    assert_eq!(
        status.code(),
        Some(0),
        "broken pipe must stop cleanly, not exit 70"
    );
}

// T-058: a worker that aborts (SIGABRT, uncatchable by catch_unwind) must not
// kill the whole batch. The parent runs each file in its own subprocess, so the
// crashed file is reported and the sibling valid file is still analyzed. The
// crash is loud: it raises the exit code to 70 (EX_SOFTWARE), which dominates
// the valid sibling's blocking 2, so an analyzer crash is never a silent pass.
// LITMUS_FORCE_ABORT is debug-gated in analyze_files, so this test is likewise
// gated. The sibling-naming assertion makes the isolation proof non-vacuous:
// the valid file must still be analyzed despite the crash next to it.
#[cfg(debug_assertions)]
#[test]
fn worker_abort_isolated_sibling_analyzed() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("crash.test.ts"),
        r#"test("triggers the forced abort hook", () => { expect(result).toBe(1) })"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg(dir.path())
        .env("LITMUS_FORCE_ABORT", "crash.test.ts")
        .output()
        .expect("failed to run litmus");

    assert_eq!(
        output.status.code(),
        Some(70),
        "a worker SIGABRT must surface loudly as exit 70, dominating the sibling's 2"
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("crash.test.ts"),
        "crashed file must be named on stderr: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("valid.test.ts"),
        "sibling valid file must still be analyzed: {stdout}"
    );
}

// T-058J: the json variant of the isolation contract. The crashed worker is
// merged into the errors array as a crash-class error (kind "crash", distinct
// from "parse"), exit 70 is raised, and stderr stays empty so the json document
// remains the sole output stream.
#[cfg(debug_assertions)]
#[test]
fn worker_abort_isolated_json() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("crash.test.ts"),
        r#"test("triggers the forced abort hook", () => { expect(result).toBe(1) })"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .env("LITMUS_FORCE_ABORT", "crash.test.ts")
        .output()
        .expect("failed to run litmus");

    assert_eq!(
        output.status.code(),
        Some(70),
        "a worker crash must surface loudly as exit 70"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("crash.test.ts"),
        "crashed file must appear in the merged errors: {stdout}"
    );
    assert!(
        stdout.contains(r#""kind":"crash""#),
        "crash must be recorded as a crash-class error, distinct from parse: {stdout}"
    );
    assert!(
        output.stderr.is_empty(),
        "json mode keeps stderr empty: {:?}",
        output.stderr
    );
}

// T-059: a worker that fails to LAUNCH (not one that crashes after launch) must
// be isolated the same way. LITMUS_FORCE_SPAWN_FAIL forces the parent's spawn
// call to return Err for the matching file, simulating EAGAIN/ENOMEM near a
// process limit. The parent must synthesize a crash-class error, exit 70, and
// still analyze the sibling — proving a single launch failure does not abort the
// whole batch (the pre-fix `?` propagation discarded all collected results).
#[cfg(debug_assertions)]
#[test]
fn worker_spawn_failure_isolated_sibling_analyzed() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("nolaunch.test.ts"),
        r#"test("never launches its worker", () => { expect(result).toBe(1) })"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg(dir.path())
        .env("LITMUS_FORCE_SPAWN_FAIL", "nolaunch.test.ts")
        .output()
        .expect("failed to run litmus");

    assert_eq!(
        output.status.code(),
        Some(70),
        "a spawn failure must surface loudly as exit 70, not abort or be silent"
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("nolaunch.test.ts"),
        "file whose worker failed to launch must be named on stderr: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("valid.test.ts"),
        "sibling must still be analyzed despite the spawn failure: {stdout}"
    );
}

// T-059J: the json variant of the spawn-failure isolation contract. The failed
// launch becomes a crash-class error fragment in the merged document, exit 70 is
// raised, and stderr stays empty.
#[cfg(debug_assertions)]
#[test]
fn worker_spawn_failure_isolated_json() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("nolaunch.test.ts"),
        r#"test("never launches its worker", () => { expect(result).toBe(1) })"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("valid.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .env("LITMUS_FORCE_SPAWN_FAIL", "nolaunch.test.ts")
        .output()
        .expect("failed to run litmus");

    assert_eq!(
        output.status.code(),
        Some(70),
        "a spawn failure must surface loudly as exit 70"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("nolaunch.test.ts"),
        "failed-launch file must appear in the merged errors: {stdout}"
    );
    assert!(
        stdout.contains(r#""kind":"crash""#),
        "spawn failure must be recorded as a crash-class error: {stdout}"
    );
    assert!(
        stdout.contains("valid.test.ts"),
        "sibling must still be merged despite the spawn failure: {stdout}"
    );
    assert!(
        output.stderr.is_empty(),
        "json mode keeps stderr empty: {:?}",
        output.stderr
    );
}

// T-J14: issues from multiple files merge into one json document. With per-file
// workers, this exercises the cross-child fragment merge directly: dropping or
// double-wrapping any worker's fragment would fail this.
#[test]
fn json_merges_issues_from_multiple_files() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("a.test.ts"),
        r#"test("weak only", () => { expect(x).toBeTruthy() })"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("b.test.ts"),
        r#"test("also weak only here", () => { expect(y).toBeTruthy() })"#,
    )
    .unwrap();

    let output = litmus_cmd()
        .arg("--json")
        .arg(dir.path())
        .output()
        .expect("failed to run litmus");

    assert_eq!(output.status.code(), Some(2));

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with(r#"{"issues":["#),
        "must be a single json document: {stdout}"
    );
    assert!(stdout.contains("a.test.ts"), "missing file a: {stdout}");
    assert!(stdout.contains("b.test.ts"), "missing file b: {stdout}");
    assert!(
        output.stderr.is_empty(),
        "json mode keeps stderr empty: {:?}",
        output.stderr
    );
}
