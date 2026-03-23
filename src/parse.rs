use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum TestModifier {
    Skip,
    Todo,
    Only,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssertionContext {
    TopLevel,
    IfBranch,
    TryBlock,
    CatchBlock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestBlock {
    pub name: String,
    pub line: u32,
    pub assertions: Vec<Assertion>,
    pub mock_calls: Vec<MockCall>,
    pub modifier: Option<TestModifier>,
    pub has_empty_body: bool,
    pub catch_swallows: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TargetKind {
    Literal,
    Identifier,
    CallResult,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assertion {
    pub line: u32,
    pub target: String,
    pub target_kind: TargetKind,
    pub matcher: String,
    pub is_weak: bool,
    pub context: AssertionContext,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MockCall {
    pub line: u32,
    pub kind: MockKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MockKind {
    ViFn,
    ViMock,
    ViSpyOn,
    BunMock,
}

// toBeNull/toBeUndefined are specific value checks (≡ toBe(null)/toBe(undefined)),
// not weak assertions like toBeTruthy/toBeDefined/toBeFalsy.
const WEAK_MATCHERS: &[&str] = &["toBeTruthy", "toBeDefined", "toBeFalsy"];

pub fn parse_test_file(source: &str, path: &Path) -> Result<Vec<TestBlock>, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path)
        .unwrap_or_else(|_| SourceType::from_path("test.ts").unwrap());
    let ret = Parser::new(&allocator, source, source_type).parse();

    if !ret.errors.is_empty() {
        let msg = ret
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(msg);
    }

    let mut blocks = Vec::new();
    walk_statements(&ret.program.body, source, &mut blocks);
    Ok(blocks)
}

fn walk_statements(stmts: &[Statement<'_>], source: &str, blocks: &mut Vec<TestBlock>) {
    for stmt in stmts {
        if let Statement::ExpressionStatement(expr_stmt) = stmt {
            check_test_call(&expr_stmt.expression, source, blocks);
        }
    }
}

fn check_test_call(expr: &Expression<'_>, source: &str, blocks: &mut Vec<TestBlock>) {
    let Expression::CallExpression(call) = expr else {
        return;
    };

    let (name, modifier) = match callee_name(&call.callee) {
        Some(pair) => pair,
        None => return,
    };

    match name {
        "test" | "it" => {
            if let Some(mut block) = extract_test_block(call, source) {
                block.modifier = modifier;
                blocks.push(block);
            } else if modifier == Some(TestModifier::Todo) {
                // test.todo("x") has no callback — create minimal block
                if let Some(name) = first_string_arg(&call.arguments) {
                    let line = offset_to_line(source, call.span.start);
                    blocks.push(TestBlock {
                        name,
                        line,
                        assertions: Vec::new(),
                        mock_calls: Vec::new(),
                        modifier: Some(TestModifier::Todo),
                        has_empty_body: true,
                        catch_swallows: Vec::new(),
                    });
                }
            }
        }
        "describe" => {
            if let Some(body) = callback_body(&call.arguments) {
                walk_statements(&body.statements, source, blocks);
            }
        }
        _ => {}
    }
}

fn callee_name<'a>(expr: &'a Expression<'a>) -> Option<(&'a str, Option<TestModifier>)> {
    match expr {
        Expression::Identifier(id) => Some((&id.name, None)),
        Expression::StaticMemberExpression(member) => {
            let modifier = match &*member.property.name {
                "skip" => Some(TestModifier::Skip),
                "todo" => Some(TestModifier::Todo),
                "only" => Some(TestModifier::Only),
                _ => return None,
            };
            if let Expression::Identifier(id) = &member.object {
                return Some((&id.name, modifier));
            }
            None
        }
        _ => None,
    }
}

fn extract_test_block(call: &CallExpression<'_>, source: &str) -> Option<TestBlock> {
    let name = first_string_arg(&call.arguments)?;
    let body = callback_body(&call.arguments)?;
    let line = offset_to_line(source, call.span.start);
    let has_empty_body = body.statements.is_empty();

    let mut assertions = Vec::new();
    let mut mock_calls = Vec::new();
    let mut catch_swallows = Vec::new();
    scan_body(
        &body.statements,
        source,
        &mut assertions,
        &mut mock_calls,
        &mut catch_swallows,
        AssertionContext::TopLevel,
    );

    Some(TestBlock {
        name,
        line,
        assertions,
        mock_calls,
        modifier: None,
        has_empty_body,
        catch_swallows,
    })
}

fn first_string_arg(args: &[Argument<'_>]) -> Option<String> {
    match args.first()? {
        Argument::StringLiteral(s) => Some(s.value.to_string()),
        _ => None,
    }
}

fn callback_body<'a>(args: &'a [Argument<'a>]) -> Option<&'a FunctionBody<'a>> {
    match args.get(1)? {
        Argument::ArrowFunctionExpression(arrow) => Some(&arrow.body),
        Argument::FunctionExpression(func) => func.body.as_deref(),
        _ => None,
    }
}

fn scan_body(
    stmts: &[Statement<'_>],
    source: &str,
    assertions: &mut Vec<Assertion>,
    mocks: &mut Vec<MockCall>,
    catch_swallows: &mut Vec<u32>,
    context: AssertionContext,
) {
    for stmt in stmts {
        scan_statement(stmt, source, assertions, mocks, catch_swallows, &context);
    }
}

fn scan_statement(
    stmt: &Statement<'_>,
    source: &str,
    assertions: &mut Vec<Assertion>,
    mocks: &mut Vec<MockCall>,
    catch_swallows: &mut Vec<u32>,
    context: &AssertionContext,
) {
    match stmt {
        Statement::ExpressionStatement(es) => {
            scan_expr(&es.expression, source, assertions, mocks, context);
        }
        Statement::VariableDeclaration(vd) => {
            for decl in &vd.declarations {
                if let Some(init) = &decl.init {
                    scan_expr(init, source, assertions, mocks, context);
                }
            }
        }
        Statement::ReturnStatement(rs) => {
            if let Some(arg) = &rs.argument {
                scan_expr(arg, source, assertions, mocks, context);
            }
        }
        Statement::BlockStatement(bs) => {
            scan_body(&bs.body, source, assertions, mocks, catch_swallows, context.clone());
        }
        Statement::IfStatement(if_stmt) => {
            scan_statement(&if_stmt.consequent, source, assertions, mocks, catch_swallows, &AssertionContext::IfBranch);
            if let Some(alt) = &if_stmt.alternate {
                scan_statement(alt, source, assertions, mocks, catch_swallows, &AssertionContext::IfBranch);
            }
        }
        Statement::ForStatement(for_stmt) => {
            scan_statement(&for_stmt.body, source, assertions, mocks, catch_swallows, context);
        }
        Statement::ForInStatement(for_in) => {
            scan_statement(&for_in.body, source, assertions, mocks, catch_swallows, context);
        }
        Statement::ForOfStatement(for_of) => {
            scan_statement(&for_of.body, source, assertions, mocks, catch_swallows, context);
        }
        Statement::WhileStatement(while_stmt) => {
            scan_statement(&while_stmt.body, source, assertions, mocks, catch_swallows, context);
        }
        Statement::DoWhileStatement(do_while) => {
            scan_statement(&do_while.body, source, assertions, mocks, catch_swallows, context);
        }
        Statement::TryStatement(try_stmt) => {
            scan_try_statement(try_stmt, source, assertions, mocks, catch_swallows, context);
        }
        Statement::SwitchStatement(switch_stmt) => {
            for case in &switch_stmt.cases {
                scan_body(&case.consequent, source, assertions, mocks, catch_swallows, context.clone());
            }
        }
        _ => {}
    }
}

fn scan_try_statement(
    try_stmt: &TryStatement<'_>,
    source: &str,
    assertions: &mut Vec<Assertion>,
    mocks: &mut Vec<MockCall>,
    catch_swallows: &mut Vec<u32>,
    context: &AssertionContext,
) {
    scan_body(&try_stmt.block.body, source, assertions, mocks, catch_swallows, AssertionContext::TryBlock);

    if let Some(handler) = &try_stmt.handler {
        if handler.body.body.is_empty() {
            catch_swallows.push(offset_to_line(source, handler.span.start));
        } else {
            let mut catch_assertions = Vec::new();
            for catch_stmt in &handler.body.body {
                scan_statement(catch_stmt, source, &mut catch_assertions, mocks, catch_swallows, &AssertionContext::CatchBlock);
            }
            if catch_assertions.is_empty()
                && !handler.body.body.iter().any(|s| matches!(s, Statement::ThrowStatement(_)))
            {
                catch_swallows.push(offset_to_line(source, handler.span.start));
            }
            assertions.extend(catch_assertions);
        }
    }

    if let Some(finalizer) = &try_stmt.finalizer {
        scan_body(&finalizer.body, source, assertions, mocks, catch_swallows, context.clone());
    }
}

fn scan_expr(
    expr: &Expression<'_>,
    source: &str,
    assertions: &mut Vec<Assertion>,
    mocks: &mut Vec<MockCall>,
    context: &AssertionContext,
) {
    match expr {
        Expression::CallExpression(call) => {
            if let Some(a) = try_assertion(call, source, context) {
                assertions.push(a);
            } else if let Some(m) = try_mock(call, source) {
                mocks.push(m);
            } else {
                // Recurse into callee for chained calls like vi.fn().mockReturnValue()
                scan_expr(&call.callee, source, assertions, mocks, context);
            }
        }
        Expression::StaticMemberExpression(member) => {
            scan_expr(&member.object, source, assertions, mocks, context);
        }
        Expression::AwaitExpression(ae) => {
            scan_expr(&ae.argument, source, assertions, mocks, context);
        }
        _ => {}
    }
}

fn try_assertion(call: &CallExpression<'_>, source: &str, context: &AssertionContext) -> Option<Assertion> {
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return None;
    };

    let (target, target_kind) = find_expect_target(&member.object, source)?;
    let matcher = member.property.name.to_string();
    let is_weak = WEAK_MATCHERS.contains(&matcher.as_str());
    let line = offset_to_line(source, call.span.start);

    Some(Assertion {
        line,
        target,
        target_kind,
        matcher,
        is_weak,
        context: context.clone(),
    })
}

fn find_expect_target(expr: &Expression<'_>, source: &str) -> Option<(String, TargetKind)> {
    match expr {
        Expression::CallExpression(call) => {
            if matches!(callee_name(&call.callee), Some(("expect", _))) {
                let (target, kind) = call
                    .arguments
                    .first()
                    .map(|arg| {
                        let text = arg.span().source_text(source).to_string();
                        let kind = classify_argument(arg);
                        (text, kind)
                    })
                    .unwrap_or_else(|| (String::new(), TargetKind::Other));
                Some((target, kind))
            } else {
                None
            }
        }
        Expression::StaticMemberExpression(member) => {
            find_expect_target(&member.object, source)
        }
        _ => None,
    }
}

fn classify_argument(arg: &Argument<'_>) -> TargetKind {
    match arg {
        Argument::BooleanLiteral(_)
        | Argument::NumericLiteral(_)
        | Argument::StringLiteral(_)
        | Argument::NullLiteral(_) => TargetKind::Literal,
        Argument::Identifier(_) => TargetKind::Identifier,
        Argument::CallExpression(_) => TargetKind::CallResult,
        Argument::AwaitExpression(ae) => classify_expression(&ae.argument),
        Argument::ParenthesizedExpression(pe) => classify_expression(&pe.expression),
        _ => TargetKind::Other,
    }
}

fn classify_expression(expr: &Expression<'_>) -> TargetKind {
    match expr {
        Expression::BooleanLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::NullLiteral(_) => TargetKind::Literal,
        Expression::Identifier(_) => TargetKind::Identifier,
        Expression::CallExpression(_) => TargetKind::CallResult,
        Expression::AwaitExpression(ae) => classify_expression(&ae.argument),
        Expression::ParenthesizedExpression(pe) => classify_expression(&pe.expression),
        _ => TargetKind::Other,
    }
}

fn try_mock(call: &CallExpression<'_>, source: &str) -> Option<MockCall> {
    let kind = match &call.callee {
        Expression::StaticMemberExpression(member) => {
            let Expression::Identifier(obj) = &member.object else {
                return None;
            };
            if obj.name != "vi" {
                return None;
            }
            match &*member.property.name {
                "fn" => MockKind::ViFn,
                "mock" => MockKind::ViMock,
                "spyOn" => MockKind::ViSpyOn,
                _ => return None,
            }
        }
        Expression::Identifier(id) if id.name == "mock" => MockKind::BunMock,
        _ => return None,
    };
    let line = offset_to_line(source, call.span.start);
    Some(MockCall { line, kind })
}

fn offset_to_line(source: &str, offset: u32) -> u32 {
    let end = (offset as usize).min(source.len());
    source[..end]
        .bytes()
        .filter(|&b| b == b'\n')
        .count() as u32
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(source: &str) -> Vec<TestBlock> {
        parse_test_file(source, Path::new("test.tsx")).unwrap()
    }

    // T-001: simple test with assertion
    #[test]
    fn parses_simple_test_block() {
        let blocks = parse(r#"test("should work", () => { expect(result).toBe(1) })"#);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "should work");
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].matcher, "toBe");
        assert!(!blocks[0].assertions[0].is_weak);
    }

    // T-002: it() recognized as test
    #[test]
    fn parses_it_as_test() {
        let blocks = parse(r#"it("works", () => { expect(x).toBe(1) })"#);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "works");
    }

    // T-003: nested describe
    #[test]
    fn parses_nested_describe() {
        let source = r#"
describe("outer", () => {
    describe("inner", () => {
        test("works", () => {
            expect(x).toBe(1)
        })
    })
})"#;
        let blocks = parse(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "works");
    }

    // T-004: weak assertion classified
    #[test]
    fn classifies_weak_assertion() {
        let blocks = parse(r#"test("x", () => { expect(x).toBeTruthy() })"#);
        assert!(blocks[0].assertions[0].is_weak);
    }

    // T-005: strong assertion classified
    #[test]
    fn classifies_strong_assertion() {
        let blocks = parse(r#"test("x", () => { expect(x).toBe(42) })"#);
        assert!(!blocks[0].assertions[0].is_weak);
    }

    // T-006: mixed weak and strong
    #[test]
    fn mixed_weak_and_strong() {
        let source = r#"test("x", () => {
            expect(x).toBeTruthy()
            expect(y).toBe(1)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 2);
        assert!(blocks[0].assertions[0].is_weak);
        assert!(!blocks[0].assertions[1].is_weak);
    }

    // T-010: mock and assertion counting
    #[test]
    fn counts_mocks_and_assertions() {
        let source = r#"test("x", () => {
            const a = vi.fn()
            const b = vi.fn()
            const c = vi.fn()
            expect(result).toBe(1)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].mock_calls.len(), 3);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-013: vi.spyOn + vi.mock
    #[test]
    fn counts_spy_and_mock() {
        let source = r#"test("x", () => {
            vi.spyOn(obj, "method")
            vi.mock("./module")
            expect(result).toBe(1)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].mock_calls.len(), 2);
        assert_eq!(blocks[0].mock_calls[0].kind, MockKind::ViSpyOn);
        assert_eq!(blocks[0].mock_calls[1].kind, MockKind::ViMock);
    }

    // T-014: Bun mock()
    #[test]
    fn detects_bun_mock() {
        let source = r#"test("x", () => {
            const mockFn = mock(() => 42)
            expect(mockFn()).toBe(42)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].mock_calls.len(), 1);
        assert_eq!(blocks[0].mock_calls[0].kind, MockKind::BunMock);
    }

    // Chained mock: vi.fn().mockReturnValue()
    #[test]
    fn detects_chained_mock() {
        let source = r#"test("x", () => {
            const mockFn = vi.fn().mockReturnValue(42)
            expect(mockFn()).toBe(42)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].mock_calls.len(), 1);
        assert_eq!(blocks[0].mock_calls[0].kind, MockKind::ViFn);
    }

    // expect().not.matcher() handled
    #[test]
    fn handles_not_modifier() {
        let source = r#"test("x", () => { expect(x).not.toBe(42) })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].matcher, "toBe");
        assert!(!blocks[0].assertions[0].is_weak);
    }

    // Weak matchers: toBeTruthy, toBeDefined, toBeFalsy
    #[test]
    fn classifies_weak_matchers() {
        for matcher in ["toBeTruthy", "toBeDefined", "toBeFalsy"] {
            let source = format!(r#"test("x", () => {{ expect(x).{matcher}() }})"#);
            let blocks = parse(&source);
            assert!(
                blocks[0].assertions[0].is_weak,
                "{matcher} should be weak"
            );
        }
    }

    // toBeNull/toBeUndefined are specific value checks, not weak
    #[test]
    fn tobenull_tobeundefined_not_weak() {
        for matcher in ["toBeNull", "toBeUndefined"] {
            let source = format!(r#"test("x", () => {{ expect(x).{matcher}() }})"#);
            let blocks = parse(&source);
            assert!(
                !blocks[0].assertions[0].is_weak,
                "{matcher} should not be weak"
            );
        }
    }

    // RC-002: function() callback (not just arrow functions)
    #[test]
    fn parses_function_callback() {
        let blocks = parse(r#"test("x", function() { expect(x).toBe(1) })"#);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // RC-002: describe with function() callback
    #[test]
    fn parses_describe_function_callback() {
        let source = r#"describe("suite", function() {
            test("x", () => { expect(x).toBe(1) })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks.len(), 1);
    }

    // RC-002: line number calculation on multiline source
    #[test]
    fn correct_line_number() {
        let source = "// line 1\n// line 2\ntest(\"x\", () => { expect(x).toBe(1) })";
        let blocks = parse(source);
        assert_eq!(blocks[0].line, 3);
    }

    // TC-005: non-vi member expressions ignored as mocks
    #[test]
    fn ignores_non_vi_member_fn() {
        let source = r#"test("x", () => {
            jest.fn()
            expect(x).toBe(1)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].mock_calls.len(), 0);
    }

    // RC-001: assertions inside if block (traversal completeness)
    #[test]
    fn finds_assertions_in_if_block() {
        let source = r#"test("x", () => {
            if (condition) {
                expect(x).toBe(1)
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // RC-001: assertions inside try/catch
    #[test]
    fn finds_assertions_in_try_catch() {
        let source = r#"test("x", () => {
            try {
                expect(x).toBe(1)
            } catch (e) {
                expect(e).toBeDefined()
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 2);
    }

    // RC-001: assertions inside for loop
    #[test]
    fn finds_assertions_in_for_loop() {
        let source = r#"test("x", () => {
            for (const item of items) {
                expect(item).toBeTruthy()
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // RC-001: describe.only recognized
    #[test]
    fn recognizes_describe_only() {
        let source = r#"describe.only("suite", () => {
            test("x", () => { expect(x).toBe(1) })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks.len(), 1);
    }

    // TC-005: .resolves modifier handled + target field verified
    #[test]
    fn handles_resolves_modifier() {
        let source = r#"test("x", async () => { await expect(promise).resolves.toBe(42) })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].matcher, "toBe");
        assert_eq!(blocks[0].assertions[0].target, "promise");
        assert!(!blocks[0].assertions[0].is_weak);
    }

    // TC-005: .rejects modifier handled
    #[test]
    fn handles_rejects_modifier() {
        let source =
            r#"test("x", async () => { await expect(badCall()).rejects.toThrow("err") })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].matcher, "toThrow");
        assert_eq!(blocks[0].assertions[0].target, "badCall()");
    }

    // TC-005: target field captured correctly
    #[test]
    fn captures_assertion_target() {
        let blocks = parse(r#"test("x", () => { expect(fetchUser(id)).toBe(42) })"#);
        assert_eq!(blocks[0].assertions[0].target, "fetchUser(id)");
    }

    // TC-009: template literal test names silently skipped
    #[test]
    fn skips_template_literal_test_names() {
        let source = "test(`dynamic ${name}`, () => { expect(x).toBe(1) })";
        let blocks = parse(source);
        assert_eq!(blocks.len(), 0);
    }

    // T-024..T-030, T-041, T-042: target kind classification
    #[test]
    fn target_kind_classification() {
        let cases: Vec<(&str, TargetKind)> = vec![
            (r#"test("x", () => { expect(true).toBe(true) })"#, TargetKind::Literal),
            (r#"test("x", () => { expect(42).toBe(42) })"#, TargetKind::Literal),
            (r#"test("x", () => { expect("hello").toEqual("hello") })"#, TargetKind::Literal),
            (r#"test("x", () => { expect(null).toBeNull() })"#, TargetKind::Literal),
            (r#"test("x", () => { expect(result).toBe(42) })"#, TargetKind::Identifier),
            (r#"test("x", () => { expect(fetchUser(1)).toBe(42) })"#, TargetKind::CallResult),
            (r#"test("x", () => { expect(obj.prop).toBe(42) })"#, TargetKind::Other),
            (r#"test("x", async () => { expect(await fn()).toBe(1) })"#, TargetKind::CallResult),
            (r#"test("x", () => { expect((result)).toBe(1) })"#, TargetKind::Identifier),
        ];
        for (source, expected) in cases {
            let blocks = parse(source);
            assert_eq!(
                blocks[0].assertions[0].target_kind, expected,
                "source: {source}"
            );
        }
    }

    // T-101: test.skip → modifier == Some(Skip)
    #[test]
    fn modifier_test_skip() {
        let blocks = parse(r#"test.skip("x", () => { expect(x).toBe(1) })"#);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].modifier, Some(TestModifier::Skip));
    }

    // T-102: test.todo → modifier == Some(Todo)
    #[test]
    fn modifier_test_todo() {
        let blocks = parse(r#"test.todo("x")"#);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].modifier, Some(TestModifier::Todo));
    }

    // T-103: test.only → modifier == Some(Only)
    #[test]
    fn modifier_test_only() {
        let blocks = parse(r#"test.only("x", () => { expect(x).toBe(1) })"#);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].modifier, Some(TestModifier::Only));
    }

    // T-104: plain test → modifier == None
    #[test]
    fn modifier_plain_test() {
        let blocks = parse(r#"test("x", () => { expect(x).toBe(1) })"#);
        assert_eq!(blocks[0].modifier, None);
    }

    // T-105: it.skip → modifier == Some(Skip)
    #[test]
    fn modifier_it_skip() {
        let blocks = parse(r#"it.skip("x", () => { expect(x).toBe(1) })"#);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].modifier, Some(TestModifier::Skip));
    }

    // T-106: top-level assertion → TopLevel
    #[test]
    fn context_top_level() {
        let blocks = parse(r#"test("x", () => { expect(x).toBe(1) })"#);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::TopLevel);
    }

    // T-107: assertion inside if → IfBranch
    #[test]
    fn context_if_branch() {
        let source = r#"test("x", () => {
            if (condition) {
                expect(x).toBe(1)
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::IfBranch);
    }

    // T-108: assertion inside try → TryBlock
    #[test]
    fn context_try_block() {
        let source = r#"test("x", () => {
            try {
                expect(x).toBe(1)
            } catch (e) {}
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::TryBlock);
    }

    // T-109: assertion inside catch → CatchBlock
    #[test]
    fn context_catch_block() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                expect(e.message).toBe("err")
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::CatchBlock);
    }

    // T-110: nested if > try > assertion → TryBlock (innermost)
    #[test]
    fn context_nested_if_try() {
        let source = r#"test("x", () => {
            if (condition) {
                try {
                    expect(x).toBe(1)
                } catch (e) {}
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::TryBlock);
    }

    // T-111: nested try > catch > if > assertion → IfBranch (innermost)
    #[test]
    fn context_nested_try_catch_if() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                if (e instanceof Error) {
                    expect(e.message).toBe("err")
                }
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::IfBranch);
    }

    // T-112: catch with throw e → no catch_swallow
    #[test]
    fn throw_in_catch_no_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                throw e
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_swallows.is_empty());
    }

    // T-113: catch with throw new Error() → no catch_swallow
    #[test]
    fn throw_new_in_catch_no_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                throw new Error("wrapped")
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_swallows.is_empty());
    }

    // T-114: empty body → has_empty_body == true
    #[test]
    fn empty_body_detected() {
        let blocks = parse(r#"test("x", () => {})"#);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].has_empty_body);
    }

    // T-115: body with assertion → has_empty_body == false
    #[test]
    fn non_empty_body() {
        let blocks = parse(r#"test("x", () => { expect(x).toBe(1) })"#);
        assert!(!blocks[0].has_empty_body);
    }

    // T-116: body with only variable decl → has_empty_body == false
    #[test]
    fn body_with_decl_not_empty() {
        let blocks = parse(r#"test("x", () => { const x = 1 })"#);
        assert!(!blocks[0].has_empty_body);
    }

    // T-117: try-catch, catch empty → catch_swallows has entry
    #[test]
    fn catch_swallow_empty_catch() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {}
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }

    // T-118: try-catch, catch with assertion → no swallow
    #[test]
    fn catch_with_assertion_no_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                expect(e).toBeDefined()
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_swallows.is_empty());
    }

    // T-119: try-catch, catch with throw → no swallow
    #[test]
    fn catch_with_throw_no_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                throw e
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_swallows.is_empty());
    }

    // T-120: catch with comment only → swallow (comments are not statements)
    #[test]
    fn catch_comment_only_is_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                // intentionally empty
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }

    // T-121: multiple try-catch, one swallows → catch_swallows.len() == 1
    #[test]
    fn multiple_try_catch_one_swallow() {
        let source = r#"test("x", () => {
            try { op1() } catch (e) {}
            try { op2() } catch (e) { throw e }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }

    // T-122: catch with console.log only → swallow
    #[test]
    fn catch_console_log_is_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                console.log(e)
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }

    // T-123: test.todo without callback → has_empty_body == true
    #[test]
    fn todo_no_callback_empty_body() {
        let blocks = parse(r#"test.todo("x")"#);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].has_empty_body);
        assert_eq!(blocks[0].modifier, Some(TestModifier::Todo));
    }

    // TC-004: try-catch-finally with assertion in finally
    #[test]
    fn finally_block_assertion_tracked() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
            } finally {
                expect(cleanup).toBe(true)
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::TopLevel);
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }
}
