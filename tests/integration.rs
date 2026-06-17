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
