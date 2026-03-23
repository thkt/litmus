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
            issues.push(Issue {
                rule: "weak-assertion",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: if block.assertions.is_empty() {
                    "no assertions".to_string()
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
                },
            });
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
            issues.push(Issue {
                rule: "mock-overuse",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: format!("mocks: {mock_count}, assertions: {assertion_count}"),
            });
        }
    }
    issues
}

pub fn check_tautological(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        for assertion in &block.assertions {
            if assertion.target_kind == TargetKind::Literal {
                issues.push(Issue {
                    rule: "tautological",
                    file: file.to_path_buf(),
                    line: assertion.line,
                    test_name: block.name.clone(),
                    detail: format!("target: {}", assertion.target),
                });
            }
        }
    }
    issues
}

const MOCK_MATCHERS: &[&str] = &[
    "toHaveBeenCalled",
    "toHaveBeenCalledWith",
    "toHaveBeenCalledTimes",
    "toHaveBeenLastCalledWith",
    "toHaveBeenNthCalledWith",
    "toHaveReturned",
    "toHaveReturnedWith",
    "toHaveReturnedTimes",
    "toHaveLastReturnedWith",
    "toHaveNthReturnedWith",
];

pub fn check_mock_only(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if !block.assertions.is_empty()
            && block.assertions.iter().all(|a| MOCK_MATCHERS.contains(&a.matcher.as_str()))
        {
            let matchers: Vec<&str> = block
                .assertions
                .iter()
                .map(|a| a.matcher.as_str())
                .collect();
            issues.push(Issue {
                rule: "mock-only",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: format!("matchers: {}", matchers.join(", ")),
            });
        }
    }
    issues
}

pub fn check_empty_test(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if block.has_empty_body {
            issues.push(Issue {
                rule: "empty-test",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: String::new(),
            });
        }
    }
    issues
}

pub fn check_skipped_test(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        if matches!(block.modifier, Some(TestModifier::Skip | TestModifier::Todo)) {
            issues.push(Issue {
                rule: "skipped-test",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: match block.modifier {
                    Some(TestModifier::Skip) => "skip".to_string(),
                    Some(TestModifier::Todo) => "todo".to_string(),
                    _ => String::new(),
                },
            });
        }
    }
    issues
}

pub fn check_catch_swallow(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        for &catch_line in &block.catch_swallows {
            issues.push(Issue {
                rule: "catch-swallow",
                file: file.to_path_buf(),
                line: catch_line,
                test_name: block.name.clone(),
                detail: "catch block has no assertions and no throw".to_string(),
            });
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
            issues.push(Issue {
                rule: "conditional-assertion",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: format!("all {} assertions inside if", block.assertions.len()),
            });
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
            issues.push(Issue {
                rule: "catch-only-assertion",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: format!("all {} assertions inside catch", block.assertions.len()),
            });
        }
    }
    issues
}

pub fn check_test_name(blocks: &[TestBlock], file: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    for block in blocks {
        let word_count = block.name.split_whitespace().count();
        if word_count <= 2 {
            issues.push(Issue {
                rule: "test-name-quality",
                file: file.to_path_buf(),
                line: block.line,
                test_name: block.name.clone(),
                detail: format!("words: {word_count}"),
            });
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
            catch_swallows: Vec::new(),
        }
    }

    fn weak_assertion(matcher: &str) -> Assertion {
        Assertion {
            line: 2,
            target: "x".into(),
            target_kind: TargetKind::Identifier,
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
        assert_eq!(
            issue.to_string(),
            "weak-assertion: test.ts:1 test case"
        );
    }

    fn literal_assertion(target: &str) -> Assertion {
        Assertion {
            line: 2,
            target: target.into(),
            target_kind: TargetKind::Literal,
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
            vec![mock_matcher_assertion("toHaveBeenCalled"), strong_assertion()],
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

    fn named_block(name: &str) -> TestBlock {
        TestBlock {
            name: name.into(),
            line: 1,
            assertions: vec![strong_assertion()],
            mock_calls: vec![],
            modifier: None,
            has_empty_body: false,
            catch_swallows: Vec::new(),
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
            catch_swallows: Vec::new(),
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
            catch_swallows: Vec::new(),
        }
    }

    fn assertion_with_context(context: AssertionContext) -> Assertion {
        Assertion {
            line: 2,
            target: "x".into(),
            target_kind: TargetKind::Identifier,
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

    // T-210: all assertions IfBranch → conditional-assertion
    #[test]
    fn conditional_assertion_all_if() {
        let b = block(vec![assertion_with_context(AssertionContext::IfBranch)], vec![]);
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
        let b = block(vec![assertion_with_context(AssertionContext::CatchBlock)], vec![]);
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
}
