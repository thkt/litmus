use crate::parse::{AssertionContext, TargetKind, TestBlock, TestModifier};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub struct Issue {
    pub rule: &'static str,
    pub file: PathBuf,
    pub line: u32,
    pub test_name: String,
    pub detail: String,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Severity {
    Warning,
    Blocking,
}

// Rules whose findings are advisory (exit code 1) rather than blocking (exit 2).
// Severity is a property of the rule, not of the individual finding, so it is
// derived from the rule name rather than stored on every Issue.
const WARNING_RULES: &[&str] = &["dummy-data", "missing-act", "snapshot-external"];

// Every rule name litmus can emit. The precision corpus gate requires a
// fire+clean fixture pair for each entry, so a new rule must be added here to
// be measured. checks_count_matches_rule_catalog pins this list's length to
// CHECKS, so a rule added to the analysis pass without a catalog entry (or vice
// versa) fails the build. It still does not verify name-by-name correspondence,
// so it guards fixture coverage, rule-name typos, and count drift — not which
// specific name maps to which check_*.
pub const RULE_CATALOG: &[&str] = &[
    "catch-masks-assertion",
    "catch-only-assertion",
    "catch-swallow",
    "conditional-assertion",
    "dummy-data",
    "empty-test",
    "missing-act",
    "mock-only",
    "mock-overuse",
    "skipped-test",
    "snapshot-external",
    "tautological",
    "test-name-quality",
    "weak-assertion",
];

// The analysis pass over every check_* rule function. analyze_source iterates
// this slice instead of hand-listing each call, so a rule is enrolled in one
// place. checks_count_matches_rule_catalog pins CHECKS.len() to RULE_CATALOG so
// the two manual lists cannot drift. Order is irrelevant: each check_* gates its
// own findings internally (e.g. empty-test via has_empty_body), so extend order
// does not affect output.
pub type CheckFn = fn(&[TestBlock], &Path) -> Vec<Issue>;
pub const CHECKS: &[CheckFn] = &[
    check_empty_test,
    check_skipped_test,
    check_catch_swallow,
    check_catch_masks_assertion,
    check_conditional_assertion,
    check_catch_only_assertion,
    check_weak_assertions,
    check_mock_overuse,
    check_tautological,
    check_mock_only,
    check_test_name,
    check_dummy_data,
    check_missing_act,
    check_snapshot_external,
];

impl Issue {
    // Build an Issue, re-owning the borrowed file path and test name internally
    // so the 14 check_* sites stop repeating `file.to_path_buf()` /
    // `block.name.clone()`. Predicates and per-rule line attribution stay at the
    // call site; only the literal's field plumbing is centralized here (#54).
    fn new(rule: &'static str, file: &Path, line: u32, test_name: &str, detail: String) -> Self {
        Self {
            rule,
            file: file.to_path_buf(),
            line,
            test_name: test_name.to_owned(),
            detail,
        }
    }

    pub fn severity(&self) -> Severity {
        if WARNING_RULES.contains(&self.rule) {
            Severity::Warning
        } else {
            Severity::Blocking
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}:{} {}",
            self.rule,
            self.file.display(),
            self.line,
            self.test_name
        )?;
        if !self.detail.is_empty() {
            write!(f, " ({})", self.detail)?;
        }
        Ok(())
    }
}

pub fn check_weak_assertions(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        // empty-test takes priority over weak-assertion (AC-1)
        if block.has_empty_body {
            continue;
        }
        if block.assertions.is_empty() || block.assertions.iter().all(|a| a.is_weak) {
            let detail = if block.assertions.is_empty() {
                "no assertions".to_owned()
            } else {
                format!(
                    "only weak: {}",
                    block
                        .assertions
                        .iter()
                        .map(|a| a.matcher.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            issues.push(Issue::new(
                "weak-assertion",
                file,
                block.line,
                &block.name,
                detail,
            ));
        }
    }
    issues
}

pub fn check_mock_overuse(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        let mock_count = block.mock_calls.len();
        let assertion_count = block.assertions.len();
        if mock_count > assertion_count {
            issues.push(Issue::new(
                "mock-overuse",
                file,
                block.line,
                &block.name,
                format!("mocks: {mock_count}, assertions: {assertion_count}"),
            ));
        }
    }
    issues
}

pub fn check_tautological(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        for assertion in &block.assertions {
            if assertion.target_kind == TargetKind::Literal {
                issues.push(Issue::new(
                    "tautological",
                    file,
                    assertion.line,
                    &block.name,
                    format!("target: {}", assertion.target),
                ));
            }
        }
    }
    issues
}

const MOCK_MATCHERS: &[&str] = &[
    "toHaveBeenCalled",
    "toHaveBeenCalledWith",
    "toHaveBeenCalledTimes",
    "toHaveBeenCalledOnce",
    "toHaveBeenCalledExactlyOnceWith",
    "toHaveBeenCalledBefore",
    "toHaveBeenCalledAfter",
    "toHaveBeenLastCalledWith",
    "toHaveBeenNthCalledWith",
    "toHaveReturned",
    "toHaveReturnedWith",
    "toHaveReturnedTimes",
    "toHaveLastReturnedWith",
    "toHaveNthReturnedWith",
    "toHaveResolved",
    "toHaveResolvedWith",
    "toHaveResolvedTimes",
    "toHaveLastResolvedWith",
    "toHaveNthResolvedWith",
];

pub fn check_mock_only(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if !block.assertions.is_empty()
            && block
                .assertions
                .iter()
                .all(|a| MOCK_MATCHERS.contains(&a.matcher.as_str()))
        {
            let matchers: Vec<&str> = block
                .assertions
                .iter()
                .map(|a| a.matcher.as_str())
                .collect();
            issues.push(Issue::new(
                "mock-only",
                file,
                block.line,
                &block.name,
                format!("matchers: {}", matchers.join(", ")),
            ));
        }
    }
    issues
}

pub fn check_empty_test(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if block.has_empty_body {
            issues.push(Issue::new(
                "empty-test",
                file,
                block.line,
                &block.name,
                String::new(),
            ));
        }
    }
    issues
}

pub fn check_skipped_test(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if matches!(
            block.modifier,
            Some(TestModifier::Skip | TestModifier::Todo)
        ) {
            let detail = match block.modifier {
                Some(TestModifier::Skip) => "skip".to_owned(),
                Some(TestModifier::Todo) => "todo".to_owned(),
                _ => String::new(),
            };
            issues.push(Issue::new(
                "skipped-test",
                file,
                block.line,
                &block.name,
                detail,
            ));
        }
    }
    issues
}

pub fn check_catch_swallow(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        for &catch_line in &block.catch_swallows {
            issues.push(Issue::new(
                "catch-swallow",
                file,
                catch_line,
                &block.name,
                "catch block has no assertions and no throw".to_owned(),
            ));
        }
    }
    issues
}

// catch-masks-assertion: a try block asserts, and the catch block also asserts
// without rethrowing, so the try assertion's thrown AssertionError is swallowed
// and replaced by a passing catch assertion — the test passes even when the try
// assertion fails (js-testing-best-practices §1.10). Disjoint from catch-swallow
// (catch has no assertion) and catch-only-assertion (try has no assertion).
pub fn check_catch_masks_assertion(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        for &catch_line in &block.catch_masks {
            issues.push(Issue::new(
                "catch-masks-assertion",
                file,
                catch_line,
                &block.name,
                "try assertion swallowed by catch; use .toThrow()/.rejects.toThrow()".to_owned(),
            ));
        }
    }
    issues
}

pub fn check_conditional_assertion(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if !block.assertions.is_empty()
            && block
                .assertions
                .iter()
                .all(|a| a.context == AssertionContext::IfBranch)
        {
            issues.push(Issue::new(
                "conditional-assertion",
                file,
                block.line,
                &block.name,
                format!("all {} assertions inside if", block.assertions.len()),
            ));
        }
    }
    issues
}

pub fn check_catch_only_assertion(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if !block.assertions.is_empty()
            && block
                .assertions
                .iter()
                .all(|a| a.context == AssertionContext::CatchBlock)
        {
            issues.push(Issue::new(
                "catch-only-assertion",
                file,
                block.line,
                &block.name,
                format!("all {} assertions inside catch", block.assertions.len()),
            ));
        }
    }
    issues
}

pub fn check_test_name(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        let word_count = block.name.split_whitespace().count();
        if word_count <= 2 {
            issues.push(Issue::new(
                "test-name-quality",
                file,
                block.line,
                &block.name,
                format!("words: {word_count}"),
            ));
        }
    }
    issues
}

pub fn check_dummy_data(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        for dummy in &block.dummy_literals {
            issues.push(Issue::new(
                "dummy-data",
                file,
                dummy.line,
                &block.name,
                format!("dummy value: {}", dummy.value),
            ));
        }
    }
    issues
}

// missing-act: a test arranges data locally but never invokes production code
// (js-testing-best-practices §1.2, the "Act" of Arrange-Act-Assert). Advisory
// (WARNING) because act detection is heuristic. Precision gates keep false
// positives down:
//   - no Act call anywhere in the body.
//   - a strong, non-literal assertion whose root identifier is a name the body
//     bound locally. Requiring the assertion to target arranged data (not a
//     hook-sourced value) is what separates the fake test `const x = 42;
//     expect(x).toBe(42)` from a well-structured test that asserts on a value
//     produced in beforeEach. Weak-only bodies belong to weak-assertion and
//     literal-target bodies to tautological, so neither fires here.
// Known limitation: an Act hidden in a computed object key (`{ [act()]: 1 }`)
// is not seen, so such a body could still fire. Rare; acceptable for v1.
pub fn check_missing_act(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if block.has_empty_body || block.has_act {
            continue;
        }
        let asserts_arranged = block.assertions.iter().any(|a| {
            !a.is_weak
                && a.target_kind != TargetKind::Literal
                && a.target_root
                    .as_ref()
                    .is_some_and(|root| block.bound_names.iter().any(|n| n == root))
        });
        if asserts_arranged {
            issues.push(Issue::new(
                "missing-act",
                file,
                block.line,
                &block.name,
                "assertions present but no Act (SUT call)".to_owned(),
            ));
        }
    }
    issues
}

// snapshot-external: a test asserts against an external snapshot file via
// toMatchSnapshot() (js-testing-best-practices §1.8). The expected value lives
// out of sight of the test, so a large external snapshot is a sign of a fragile
// test whose failures read as an opaque diff far from the assertion site.
// Advisory (WARNING) rather than blocking: a snapshot still verifies output, so
// it is fragile rather than empty like the no-verify rules. toMatchInlineSnapshot
// keeps the expected value beside the assertion and is intentionally not in the
// flag set. The set is a named const so sibling external-snapshot matchers can
// be added without touching the loop.
const SNAPSHOT_MATCHERS: &[&str] = &["toMatchSnapshot"];

pub fn check_snapshot_external(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        for assertion in &block.assertions {
            if SNAPSHOT_MATCHERS.contains(&assertion.matcher.as_str()) {
                issues.push(Issue::new(
                    "snapshot-external",
                    file,
                    assertion.line,
                    &block.name,
                    format!("matcher: {}", assertion.matcher),
                ));
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::*;

    fn path() -> &'static Path {
        Path::new("test.ts")
    }

    fn block(assertions: Vec<Assertion>, mocks: Vec<MockCall>) -> TestBlock {
        TestBlock {
            name: "test case".into(),
            line: 1,
            assertions,
            mock_calls: mocks,
            modifier: None,
            has_empty_body: false,
            has_act: true,
            bound_names: Vec::new(),
            catch_swallows: Vec::new(),
            catch_masks: Vec::new(),
            dummy_literals: Vec::new(),
        }
    }

    fn weak_assertion(matcher: &str) -> Assertion {
        Assertion {
            line: 2,
            target: "x".into(),
            target_kind: TargetKind::Identifier,
            target_root: Some("x".into()),
            matcher: matcher.into(),
            is_weak: true,
            context: AssertionContext::TopLevel,
        }
    }

    fn strong_assertion() -> Assertion {
        Assertion {
            line: 2,
            target: "x".into(),
            target_kind: TargetKind::Identifier,
            target_root: Some("x".into()),
            matcher: "toBe".into(),
            is_weak: false,
            context: AssertionContext::TopLevel,
        }
    }

    fn mock(kind: MockKind) -> MockCall {
        MockCall { line: 2, kind }
    }

    // T-007: weak assertion only → issue
    #[test]
    fn weak_only_detected() {
        let blocks = vec![block(vec![weak_assertion("toBeTruthy")], vec![])];
        let issues = check_weak_assertions(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "weak-assertion");
    }

    // T-008: meaningful assertion present → no issue
    #[test]
    fn meaningful_present_no_issue() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_weak_assertions(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-009: no assertions → issue
    #[test]
    fn no_assertions_detected() {
        let blocks = vec![block(vec![], vec![])];
        let issues = check_weak_assertions(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].detail.contains("no assertions"));
    }

    // Mixed weak + strong → no issue
    #[test]
    fn mixed_no_issue() {
        let blocks = vec![block(
            vec![weak_assertion("toBeTruthy"), strong_assertion()],
            vec![],
        )];
        let issues = check_weak_assertions(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-011: mock overuse detected
    #[test]
    fn mock_overuse_detected() {
        let blocks = vec![block(
            vec![strong_assertion()],
            vec![
                mock(MockKind::ViFn),
                mock(MockKind::ViFn),
                mock(MockKind::ViFn),
            ],
        )];
        let issues = check_mock_overuse(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].detail.contains("mocks: 3"));
        assert!(issues[0].detail.contains("assertions: 1"));
    }

    // T-012: mock not overused
    #[test]
    fn mock_not_overused() {
        let blocks = vec![block(
            vec![strong_assertion(), strong_assertion(), strong_assertion()],
            vec![mock(MockKind::ViFn)],
        )];
        let issues = check_mock_overuse(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // Equal mocks and assertions → no issue
    #[test]
    fn equal_mocks_assertions_no_issue() {
        let blocks = vec![block(vec![strong_assertion()], vec![mock(MockKind::ViFn)])];
        let issues = check_mock_overuse(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // Display format with detail
    #[test]
    fn issue_display_format() {
        let issue = Issue {
            rule: "mock-overuse",
            file: PathBuf::from("src/test.ts"),
            line: 10,
            test_name: "should fetch".into(),
            detail: "mocks: 3, assertions: 1".into(),
        };
        assert_eq!(
            issue.to_string(),
            "mock-overuse: src/test.ts:10 should fetch (mocks: 3, assertions: 1)"
        );
    }

    // TC-006: Display format without detail
    #[test]
    fn issue_display_empty_detail() {
        let issue = Issue {
            rule: "weak-assertion",
            file: PathBuf::from("test.ts"),
            line: 1,
            test_name: "test case".into(),
            detail: "".into(),
        };
        assert_eq!(issue.to_string(), "weak-assertion: test.ts:1 test case");
    }

    fn literal_assertion(target: &str) -> Assertion {
        Assertion {
            line: 2,
            target: target.into(),
            target_kind: TargetKind::Literal,
            target_root: None,
            matcher: "toBe".into(),
            is_weak: false,
            context: AssertionContext::TopLevel,
        }
    }

    fn mock_matcher_assertion(matcher: &str) -> Assertion {
        Assertion {
            line: 2,
            target: "mockFn".into(),
            target_kind: TargetKind::Identifier,
            target_root: Some("mockFn".into()),
            matcher: matcher.into(),
            is_weak: false,
            context: AssertionContext::TopLevel,
        }
    }

    // T-031: literal only → tautological
    #[test]
    fn tautological_literal_only() {
        let blocks = vec![block(vec![literal_assertion("true")], vec![])];
        let issues = check_tautological(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "tautological");
        assert!(issues[0].detail.contains("true"));
    }

    // T-032: literal + non-literal mixed → literal assertion reported individually
    #[test]
    fn tautological_mixed_reports_literal() {
        let blocks = vec![block(
            vec![literal_assertion("true"), strong_assertion()],
            vec![],
        )];
        let issues = check_tautological(&blocks, path());
        assert_eq!(issues.len(), 1);
    }

    // T-033: no literals → no issue
    #[test]
    fn tautological_no_literals() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_tautological(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-036: all mock matchers → mock-only
    #[test]
    fn mock_only_detected() {
        let blocks = vec![block(
            vec![
                mock_matcher_assertion("toHaveBeenCalledWith"),
                mock_matcher_assertion("toHaveBeenCalledTimes"),
            ],
            vec![],
        )];
        let issues = check_mock_only(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "mock-only");
    }

    // T-037: mock matcher + value matcher mixed → no issue
    #[test]
    fn mock_only_mixed_no_issue() {
        let blocks = vec![block(
            vec![
                mock_matcher_assertion("toHaveBeenCalled"),
                strong_assertion(),
            ],
            vec![],
        )];
        let issues = check_mock_only(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-038: no assertions → no issue (weak-assertion covers)
    #[test]
    fn mock_only_empty_no_issue() {
        let blocks = vec![block(vec![], vec![])];
        let issues = check_mock_only(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-039: toHaveReturnedWith → mock-only
    #[test]
    fn mock_only_return_matchers() {
        let blocks = vec![block(
            vec![mock_matcher_assertion("toHaveReturnedWith")],
            vec![],
        )];
        let issues = check_mock_only(&blocks, path());
        assert_eq!(issues.len(), 1);
    }

    // T-039b: vitest spy matchers missing from the list still count as mock-only (#30)
    #[test]
    fn mock_only_vitest_call_and_resolve_matchers() {
        for matcher in [
            "toHaveBeenCalledOnce",
            "toHaveBeenCalledExactlyOnceWith",
            "toHaveBeenCalledBefore",
            "toHaveBeenCalledAfter",
            "toHaveResolved",
            "toHaveResolvedTimes",
            "toHaveResolvedWith",
            "toHaveLastResolvedWith",
            "toHaveNthResolvedWith",
        ] {
            let blocks = vec![block(vec![mock_matcher_assertion(matcher)], vec![])];
            let issues = check_mock_only(&blocks, path());
            assert_eq!(issues.len(), 1, "{matcher} should be mock-only");
            assert_eq!(issues[0].rule, "mock-only");
        }
    }

    fn named_block(name: &str) -> TestBlock {
        TestBlock {
            name: name.into(),
            line: 1,
            assertions: vec![strong_assertion()],
            mock_calls: vec![],
            modifier: None,
            has_empty_body: false,
            has_act: true,
            bound_names: Vec::new(),
            catch_swallows: Vec::new(),
            catch_masks: Vec::new(),
            dummy_literals: Vec::new(),
        }
    }

    // T-043: 1-word test name → issue
    #[test]
    fn test_name_one_word_detected() {
        let blocks = vec![named_block("works")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "test-name-quality");
        assert!(issues[0].detail.contains("words: 1"));
    }

    // T-044: 2-word test name → issue
    #[test]
    fn test_name_two_words_detected() {
        let blocks = vec![named_block("should work")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].detail.contains("words: 2"));
    }

    // T-045: 4-word test name → no issue
    #[test]
    fn test_name_four_words_passes() {
        let blocks = vec![named_block("returns user by id")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-054: 3-word boundary → no issue (exact threshold)
    #[test]
    fn test_name_three_words_passes() {
        let blocks = vec![named_block("returns correct value")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-046: empty test name → issue (0 words)
    #[test]
    fn test_name_empty_detected() {
        let blocks = vec![named_block("")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].detail.contains("words: 0"));
    }

    // T-048: mixed blocks — only short name reported
    #[test]
    fn test_name_mixed_blocks_only_short_reported() {
        let blocks = vec![named_block("works"), named_block("returns user by id")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].test_name, "works");
    }

    // T-049: camelCase single token → issue
    #[test]
    fn test_name_camel_case_single_word() {
        let blocks = vec![named_block("getUserById")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].detail.contains("words: 1"));
    }

    // T-050: whitespace-only name → issue (0 words)
    #[test]
    fn test_name_whitespace_only_detected() {
        let blocks = vec![named_block("   ")];
        let issues = check_test_name(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].detail.contains("words: 0"));
    }

    fn empty_block() -> TestBlock {
        TestBlock {
            name: "test case".into(),
            line: 1,
            assertions: vec![],
            mock_calls: vec![],
            modifier: None,
            has_empty_body: true,
            has_act: false,
            bound_names: Vec::new(),
            catch_swallows: Vec::new(),
            catch_masks: Vec::new(),
            dummy_literals: Vec::new(),
        }
    }

    fn skipped_block(modifier: TestModifier) -> TestBlock {
        TestBlock {
            name: "test case".into(),
            line: 1,
            assertions: vec![strong_assertion()],
            mock_calls: vec![],
            modifier: Some(modifier),
            has_empty_body: false,
            has_act: true,
            bound_names: Vec::new(),
            catch_swallows: Vec::new(),
            catch_masks: Vec::new(),
            dummy_literals: Vec::new(),
        }
    }

    fn assertion_with_context(context: AssertionContext) -> Assertion {
        Assertion {
            line: 2,
            target: "x".into(),
            target_kind: TargetKind::Identifier,
            target_root: Some("x".into()),
            matcher: "toBe".into(),
            is_weak: false,
            context,
        }
    }

    // T-201: empty body → empty-test
    #[test]
    fn empty_test_detected() {
        let blocks = vec![empty_block()];
        let issues = check_empty_test(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "empty-test");
    }

    // T-202: non-empty body → no issue
    #[test]
    fn empty_test_non_empty_no_issue() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_empty_test(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-203: empty body + weak-assertion → suppressed
    #[test]
    fn weak_assertion_suppressed_for_empty_body() {
        let blocks = vec![empty_block()];
        let issues = check_weak_assertions(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-204, T-205: Skip and Todo → skipped-test
    #[test]
    fn skipped_test_detected() {
        for modifier in [TestModifier::Skip, TestModifier::Todo] {
            let blocks = vec![skipped_block(modifier)];
            let issues = check_skipped_test(&blocks, path());
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].rule, "skipped-test");
        }
    }

    // T-206: modifier Only → no issue
    #[test]
    fn skipped_test_only_no_issue() {
        let blocks = vec![skipped_block(TestModifier::Only)];
        let issues = check_skipped_test(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-207: modifier None → no issue
    #[test]
    fn skipped_test_none_no_issue() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_skipped_test(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-208: catch_swallows non-empty → catch-swallow
    #[test]
    fn catch_swallow_detected() {
        let mut b = block(vec![strong_assertion()], vec![]);
        b.catch_swallows = vec![5];
        let issues = check_catch_swallow(&[b], path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "catch-swallow");
    }

    // T-209: catch_swallows empty → no issue
    #[test]
    fn catch_swallow_empty_no_issue() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_catch_swallow(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-209b: catch_masks non-empty → catch-masks-assertion, blocking severity
    #[test]
    fn catch_masks_assertion_detected() {
        let mut b = block(vec![strong_assertion()], vec![]);
        b.catch_masks = vec![7];
        let issues = check_catch_masks_assertion(&[b], path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "catch-masks-assertion");
        assert_eq!(issues[0].line, 7);
        assert_eq!(issues[0].severity(), Severity::Blocking);
    }

    // T-209c: catch_masks empty → no issue
    #[test]
    fn catch_masks_assertion_empty_no_issue() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_catch_masks_assertion(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-210: all assertions IfBranch → conditional-assertion
    #[test]
    fn conditional_assertion_all_if() {
        let b = block(
            vec![assertion_with_context(AssertionContext::IfBranch)],
            vec![],
        );
        let issues = check_conditional_assertion(&[b], path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "conditional-assertion");
    }

    // T-211: mixed TopLevel + IfBranch → no issue
    #[test]
    fn conditional_assertion_mixed_no_issue() {
        let b = block(
            vec![
                assertion_with_context(AssertionContext::TopLevel),
                assertion_with_context(AssertionContext::IfBranch),
            ],
            vec![],
        );
        let issues = check_conditional_assertion(&[b], path());
        assert_eq!(issues.len(), 0);
    }

    // T-212: all TopLevel → no issue
    #[test]
    fn conditional_assertion_all_top_no_issue() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_conditional_assertion(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-213: no assertions → no issue
    #[test]
    fn conditional_assertion_empty_no_issue() {
        let blocks = vec![block(vec![], vec![])];
        let issues = check_conditional_assertion(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-214: all assertions CatchBlock → catch-only-assertion
    #[test]
    fn catch_only_assertion_detected() {
        let b = block(
            vec![assertion_with_context(AssertionContext::CatchBlock)],
            vec![],
        );
        let issues = check_catch_only_assertion(&[b], path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "catch-only-assertion");
    }

    // T-215: mixed TryBlock + CatchBlock → no issue
    #[test]
    fn catch_only_mixed_no_issue() {
        let b = block(
            vec![
                assertion_with_context(AssertionContext::TryBlock),
                assertion_with_context(AssertionContext::CatchBlock),
            ],
            vec![],
        );
        let issues = check_catch_only_assertion(&[b], path());
        assert_eq!(issues.len(), 0);
    }

    // T-216: no assertions → no issue
    #[test]
    fn catch_only_empty_no_issue() {
        let blocks = vec![block(vec![], vec![])];
        let issues = check_catch_only_assertion(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    fn dummy_literal(value: &str, line: u32) -> DummyLiteral {
        DummyLiteral {
            value: value.into(),
            line,
        }
    }

    fn block_with_dummies(dummies: Vec<DummyLiteral>) -> TestBlock {
        TestBlock {
            name: "test case".into(),
            line: 1,
            assertions: vec![],
            mock_calls: vec![],
            modifier: None,
            has_empty_body: false,
            has_act: true,
            bound_names: Vec::new(),
            catch_swallows: Vec::new(),
            catch_masks: Vec::new(),
            dummy_literals: dummies,
        }
    }

    // T-230: dummy literals → one issue per literal at the literal's line
    #[test]
    fn dummy_data_one_issue_per_literal() {
        let b = block_with_dummies(vec![dummy_literal("foo", 2), dummy_literal("bar", 3)]);
        let issues = check_dummy_data(&[b], path());
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|i| i.rule == "dummy-data"));
        assert_eq!(issues[0].line, 2);
        assert_eq!(issues[1].line, 3);
    }

    // T-231: no dummy literals → no issue
    #[test]
    fn dummy_data_empty_no_issue() {
        let issues = check_dummy_data(&[block_with_dummies(vec![])], path());
        assert_eq!(issues.len(), 0);
    }

    // T-232: detail names the matched value
    #[test]
    fn dummy_data_detail_names_value() {
        let b = block_with_dummies(vec![dummy_literal("foo", 2)]);
        let issues = check_dummy_data(&[b], path());
        assert!(issues[0].detail.contains("foo"));
    }

    // T-233: dummy-data is warning severity
    #[test]
    fn dummy_data_severity_is_warning() {
        let b = block_with_dummies(vec![dummy_literal("foo", 2)]);
        let issues = check_dummy_data(&[b], path());
        assert_eq!(issues[0].severity(), Severity::Warning);
    }

    // T-234: another rule is blocking severity
    #[test]
    fn other_rule_severity_is_blocking() {
        let issues = check_weak_assertions(&[block(vec![], vec![])], path());
        assert_eq!(issues[0].severity(), Severity::Blocking);
    }

    // T-011: RULE_CATALOG lists 14 rule names with no duplicates.
    #[test]
    fn rule_catalog_has_fourteen_unique_rules() {
        assert_eq!(RULE_CATALOG.len(), 14);
        let mut sorted = RULE_CATALOG.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 14, "RULE_CATALOG has duplicate rule names");
    }

    // T-330: CHECKS enrols exactly as many functions as RULE_CATALOG names, so
    // adding a check_* without registering it in CHECKS (or vice versa) breaks
    // the build. This links the two manual lists the analysis pass depends on,
    // closing the "defined but never called" drift the #32 follow-up flagged.
    #[test]
    fn checks_count_matches_rule_catalog() {
        assert_eq!(
            CHECKS.len(),
            RULE_CATALOG.len(),
            "CHECKS and RULE_CATALOG drifted: a rule was added to one list but not the other"
        );
    }

    // T-010: every WARNING_RULES entry is present in RULE_CATALOG, so a warning
    // rule cannot exist outside the catalog the precision corpus enumerates.
    #[test]
    fn warning_rules_are_subset_of_catalog() {
        for rule in WARNING_RULES {
            assert!(
                RULE_CATALOG.contains(rule),
                "warning rule {rule} missing from RULE_CATALOG"
            );
        }
    }

    // T-235: every rule listed in WARNING_RULES resolves to Warning, so a new
    // warning rule added to the list cannot silently default to Blocking.
    #[test]
    fn all_warning_rules_resolve_to_warning() {
        for rule in WARNING_RULES {
            let issue = Issue {
                rule,
                file: path().to_path_buf(),
                line: 1,
                test_name: "case".to_owned(),
                detail: String::new(),
            };
            assert_eq!(
                issue.severity(),
                Severity::Warning,
                "rule {rule} should warn"
            );
        }
    }

    // Builds a block that binds the given names but makes no SUT call
    // (has_act false) — the shape check_missing_act targets. The assertion
    // helpers target "x", so bind "x" to model arranged-and-asserted data.
    fn act_block(has_act: bool, bound: &[&str], assertions: Vec<Assertion>) -> TestBlock {
        TestBlock {
            name: "test case".into(),
            line: 1,
            assertions,
            mock_calls: Vec::new(),
            modifier: None,
            has_empty_body: false,
            has_act,
            bound_names: bound.iter().map(|s| (*s).to_owned()).collect(),
            catch_swallows: Vec::new(),
            catch_masks: Vec::new(),
            dummy_literals: Vec::new(),
        }
    }

    // T-240: local arrange + strong assertion on the bound name + no Act → issue
    #[test]
    fn missing_act_arrange_only_detected() {
        let blocks = vec![act_block(false, &["x"], vec![strong_assertion()])];
        let issues = check_missing_act(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "missing-act");
        assert!(issues[0].detail.contains("no Act"));
    }

    // T-241: an Act call present → no issue
    #[test]
    fn missing_act_with_act_no_issue() {
        let blocks = vec![act_block(true, &["x"], vec![strong_assertion()])];
        let issues = check_missing_act(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-242: no local binding (arrange/act live in setup hooks) → no issue
    #[test]
    fn missing_act_no_local_binding_no_issue() {
        let blocks = vec![act_block(false, &[], vec![strong_assertion()])];
        let issues = check_missing_act(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-247: assertion targets a hook-sourced value, not the local binding →
    // no issue (the binding and the assertion target are decoupled).
    #[test]
    fn missing_act_assertion_targets_unbound_name_no_issue() {
        let blocks = vec![act_block(false, &["expected"], vec![strong_assertion()])];
        let issues = check_missing_act(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-243: weak-only assertion defers to weak-assertion → no issue
    #[test]
    fn missing_act_weak_only_defers() {
        let blocks = vec![act_block(false, &["x"], vec![weak_assertion("toBeTruthy")])];
        let issues = check_missing_act(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-244: literal-target assertion defers to tautological → no issue
    #[test]
    fn missing_act_literal_target_defers() {
        let blocks = vec![act_block(false, &["lit"], vec![literal_assertion("42")])];
        let issues = check_missing_act(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-245: empty body → no issue
    #[test]
    fn missing_act_empty_body_no_issue() {
        let issues = check_missing_act(&[empty_block()], path());
        assert_eq!(issues.len(), 0);
    }

    // T-246: missing-act is warning severity
    #[test]
    fn missing_act_severity_is_warning() {
        let blocks = vec![act_block(false, &["x"], vec![strong_assertion()])];
        let issues = check_missing_act(&blocks, path());
        assert_eq!(issues[0].severity(), Severity::Warning);
    }

    fn snapshot_assertion(matcher: &str, line: u32) -> Assertion {
        Assertion {
            line,
            target: "x".into(),
            target_kind: TargetKind::Identifier,
            target_root: Some("x".into()),
            matcher: matcher.into(),
            is_weak: false,
            context: AssertionContext::TopLevel,
        }
    }

    // T-416: toMatchSnapshot flagged, reported at the assertion line with the
    // matcher in the detail.
    #[test]
    fn snapshot_external_detects_to_match_snapshot() {
        let blocks = vec![block(
            vec![snapshot_assertion("toMatchSnapshot", 7)],
            vec![],
        )];
        let issues = check_snapshot_external(&blocks, path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].rule, "snapshot-external");
        assert_eq!(issues[0].line, 7);
        assert_eq!(issues[0].detail, "matcher: toMatchSnapshot");
    }

    // T-417: inline snapshot keeps the value beside the assertion, so it is not
    // flagged.
    #[test]
    fn snapshot_external_ignores_inline_snapshot() {
        let blocks = vec![block(
            vec![snapshot_assertion("toMatchInlineSnapshot", 2)],
            vec![],
        )];
        let issues = check_snapshot_external(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-418: a non-snapshot matcher is left to the other rules.
    #[test]
    fn snapshot_external_ignores_regular_matcher() {
        let blocks = vec![block(vec![strong_assertion()], vec![])];
        let issues = check_snapshot_external(&blocks, path());
        assert_eq!(issues.len(), 0);
    }

    // T-419: each snapshot assertion in a block yields its own finding.
    #[test]
    fn snapshot_external_reports_each_assertion() {
        let blocks = vec![block(
            vec![
                snapshot_assertion("toMatchSnapshot", 3),
                snapshot_assertion("toMatchSnapshot", 5),
            ],
            vec![],
        )];
        let issues = check_snapshot_external(&blocks, path());
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[1].line, 5);
    }

    // T-420: snapshot-external is advisory (a snapshot still verifies output).
    #[test]
    fn snapshot_external_severity_is_warning() {
        let blocks = vec![block(
            vec![snapshot_assertion("toMatchSnapshot", 2)],
            vec![],
        )];
        let issues = check_snapshot_external(&blocks, path());
        assert_eq!(issues[0].severity(), Severity::Warning);
    }
}
