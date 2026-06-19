use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpression, ArrayExpressionElement, BindingPattern, CallExpression,
    ChainElement, Expression, FunctionBody, ObjectExpression, ObjectPropertyKind, Statement,
    StringLiteral, TryStatement,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum TestModifier {
    Skip,
    Todo,
    Only,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// Whether the body invokes production code (the "Act" step of AAA). False
    /// when the test only arranges data and asserts on it, with no SUT call.
    pub has_act: bool,
    /// Names the body binds locally (the "Arrange" step), e.g. `x` in
    /// `const x = 42`. missing-act fires only when an assertion targets one of
    /// these names with no Act, separating data arranged-and-asserted in the
    /// body from data whose arrange/act lives in setup hooks.
    pub bound_names: Vec<String>,
    pub catch_swallows: Vec<u32>,
    /// Catch-handler lines where the try block contains an assertion and the
    /// catch block also asserts without rethrowing. The try assertion's
    /// AssertionError is swallowed and replaced by a passing catch assertion,
    /// so the test passes even when the try assertion fails
    /// (js-testing-best-practices §1.10). Disjoint from catch_swallows (catch
    /// has no assertion) and catch-only-assertion (try has no assertion).
    pub catch_masks: Vec<u32>,
    pub dummy_literals: Vec<DummyLiteral>,
}

/// A string literal inside a test body whose value matches a known dummy
/// placeholder (js-testing-best-practices §1.6 "don't foo").
#[derive(Debug, Clone, PartialEq)]
pub struct DummyLiteral {
    pub value: String,
    pub line: u32,
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
    /// Root identifier of the assertion target, e.g. `user` in
    /// `expect(user.name)`. None when the target is a literal or call result.
    pub target_root: Option<String>,
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

// Conservative dummy-placeholder set: near-zero false-positive tokens that are
// almost never legitimate test inputs. "test"/"abc"/"xxx" and numeric patterns
// are intentionally excluded (high FP vs. the precision indicator).
const DUMMY_STRINGS: &[&str] = &["foo", "bar", "baz", "qux", "hoge", "fuga"];

// oxc's recursive-descent parser overflows the stack on pathologically nested
// input. The release floor measured for issue #25 is ~2700 levels of
// expression bracket nesting (`[[…]]`, `((…))`, `{a:{…}}`, `f(f(…))`); `if`/
// block nesting overflows higher (~5700). A stack overflow aborts the process
// with SIGABRT, which `catch_unwind` cannot intercept, so a single
// deeply-nested file would take down analysis of every other file and violate
// the ADR-0066 fault-isolation contract. A pre-parse byte scan rejects such a
// file as a parse error instead, preserving per-file isolation. 500 sits ~5x
// below the measured floor and ~10x above any realistic source nesting.
//
// The floor scales with the main-thread stack size (~3KB/level). The ~2700
// figure assumes the 8MB default of the only distributed targets (apple-darwin
// and unknown-linux-gnu, per release.yml — all Unix). A 1MB-stack platform
// (e.g. Windows) would lower the floor to ~340 and let depths 341-500 slip past
// the guard into an overflow; revisit this limit before adding such a target.
const BRACKET_DEPTH_LIMIT: usize = 500;

// Maximum `{`/`[`/`(` nesting depth in `source`, counted byte-wise without
// lexing. String, comment, and regex contents inflate the count (an unmatched
// brace in a string literal is counted), but the wide margin between the limit
// and both realistic depth (~tens) and the overflow floor (~2700) absorbs that
// false-positive risk. Only bracket-bearing constructs recurse in oxc; prefix/
// binary/member/await chains parse iteratively (verified to n=20000), so a
// bracket scan has no structural blind spot (issue #25).
fn max_bracket_depth(source: &str) -> usize {
    let mut depth: usize = 0;
    let mut max = 0;
    for &b in source.as_bytes() {
        match b {
            b'{' | b'[' | b'(' => {
                depth += 1;
                if depth > max {
                    max = depth;
                }
            }
            b'}' | b']' | b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

pub fn parse_test_file(source: &str, path: &Path) -> Result<Vec<TestBlock>, String> {
    if max_bracket_depth(source) > BRACKET_DEPTH_LIMIT {
        return Err(format!(
            "bracket nesting depth exceeds limit of {BRACKET_DEPTH_LIMIT}"
        ));
    }

    let allocator = Allocator::default();
    let source_type =
        SourceType::from_path(path).unwrap_or_else(|_| SourceType::from_path("test.ts").unwrap());
    let ret = Parser::new(&allocator, source, source_type).parse();

    if !ret.diagnostics.is_empty() {
        let msg = ret
            .diagnostics
            .iter()
            .map(ToString::to_string)
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
                        has_act: false,
                        bound_names: Vec::new(),
                        catch_swallows: Vec::new(),
                        catch_masks: Vec::new(),
                        dummy_literals: Vec::new(),
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
    let has_act = body_has_act(&body.statements);
    let bound_names = body_bound_names(&body.statements);

    let mut assertions = Vec::new();
    let mut mock_calls = Vec::new();
    let mut catch_swallows = Vec::new();
    let mut catch_masks = Vec::new();
    let mut dummy_literals = Vec::new();
    scan_body(
        &body.statements,
        source,
        &mut assertions,
        &mut mock_calls,
        &mut catch_swallows,
        &mut catch_masks,
        &mut dummy_literals,
        AssertionContext::TopLevel,
    );

    Some(TestBlock {
        name,
        line,
        assertions,
        mock_calls,
        modifier: None,
        has_empty_body,
        has_act,
        bound_names,
        catch_swallows,
        catch_masks,
        dummy_literals,
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

// Output sinks (assertions / mocks / catch_swallows / catch_masks / dummies)
// are threaded as explicit parameters, matching the established style; the
// count sits one over clippy's default after adding catch_masks.
#[allow(clippy::too_many_arguments)]
fn scan_body(
    stmts: &[Statement<'_>],
    source: &str,
    assertions: &mut Vec<Assertion>,
    mocks: &mut Vec<MockCall>,
    catch_swallows: &mut Vec<u32>,
    catch_masks: &mut Vec<u32>,
    dummies: &mut Vec<DummyLiteral>,
    context: AssertionContext,
) {
    for stmt in stmts {
        scan_statement(
            stmt,
            source,
            assertions,
            mocks,
            catch_swallows,
            catch_masks,
            dummies,
            &context,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_statement(
    stmt: &Statement<'_>,
    source: &str,
    assertions: &mut Vec<Assertion>,
    mocks: &mut Vec<MockCall>,
    catch_swallows: &mut Vec<u32>,
    catch_masks: &mut Vec<u32>,
    dummies: &mut Vec<DummyLiteral>,
    context: &AssertionContext,
) {
    match stmt {
        Statement::ExpressionStatement(es) => {
            scan_expr(&es.expression, source, assertions, mocks, context);
            collect_dummies_expr(&es.expression, source, dummies);
        }
        Statement::VariableDeclaration(vd) => {
            for decl in &vd.declarations {
                if let Some(init) = &decl.init {
                    scan_expr(init, source, assertions, mocks, context);
                    collect_dummies_expr(init, source, dummies);
                }
            }
        }
        Statement::ReturnStatement(rs) => {
            if let Some(arg) = &rs.argument {
                scan_expr(arg, source, assertions, mocks, context);
                collect_dummies_expr(arg, source, dummies);
            }
        }
        Statement::BlockStatement(bs) => {
            scan_body(
                &bs.body,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                *context,
            );
        }
        Statement::IfStatement(if_stmt) => {
            scan_statement(
                &if_stmt.consequent,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                &AssertionContext::IfBranch,
            );
            if let Some(alt) = &if_stmt.alternate {
                scan_statement(
                    alt,
                    source,
                    assertions,
                    mocks,
                    catch_swallows,
                    catch_masks,
                    dummies,
                    &AssertionContext::IfBranch,
                );
            }
        }
        Statement::ForStatement(for_stmt) => {
            scan_statement(
                &for_stmt.body,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                context,
            );
        }
        Statement::ForInStatement(for_in) => {
            scan_statement(
                &for_in.body,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                context,
            );
        }
        Statement::ForOfStatement(for_of) => {
            scan_statement(
                &for_of.body,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                context,
            );
        }
        Statement::WhileStatement(while_stmt) => {
            scan_statement(
                &while_stmt.body,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                context,
            );
        }
        Statement::DoWhileStatement(do_while) => {
            scan_statement(
                &do_while.body,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                context,
            );
        }
        Statement::TryStatement(try_stmt) => {
            scan_try_statement(
                try_stmt,
                source,
                assertions,
                mocks,
                catch_swallows,
                catch_masks,
                dummies,
                context,
            );
        }
        Statement::SwitchStatement(switch_stmt) => {
            for case in &switch_stmt.cases {
                scan_body(
                    &case.consequent,
                    source,
                    assertions,
                    mocks,
                    catch_swallows,
                    catch_masks,
                    dummies,
                    *context,
                );
            }
        }
        _ => {}
    }
}

// True when an assertion appears as a direct (top-level) statement of the try
// block. Used by catch-masks to decide whether the try contributes an
// AssertionError the catch could swallow. Nested control flow is intentionally
// excluded for consistency with the top-level catch rethrow check.
fn try_block_has_top_level_assertion(body: &[Statement<'_>], source: &str) -> bool {
    body.iter().any(|stmt| {
        let Statement::ExpressionStatement(es) = stmt else {
            return false;
        };
        let mut probe = Vec::new();
        let mut throwaway_mocks = Vec::new();
        scan_expr(
            &es.expression,
            source,
            &mut probe,
            &mut throwaway_mocks,
            &AssertionContext::TryBlock,
        );
        !probe.is_empty()
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_try_statement(
    try_stmt: &TryStatement<'_>,
    source: &str,
    assertions: &mut Vec<Assertion>,
    mocks: &mut Vec<MockCall>,
    catch_swallows: &mut Vec<u32>,
    catch_masks: &mut Vec<u32>,
    dummies: &mut Vec<DummyLiteral>,
    context: &AssertionContext,
) {
    // catch-masks judges only top-level try assertions, mirroring the top-level
    // rethrow check on the catch. An assertion inside a nested try-catch is
    // shielded by that inner catch and never reaches this catch, so a delta over
    // the body-wide flattened vec (which scan_body bubbles inner assertions into)
    // would misfire on the outer catch.
    let try_has_assertion = try_block_has_top_level_assertion(&try_stmt.block.body, source);
    scan_body(
        &try_stmt.block.body,
        source,
        assertions,
        mocks,
        catch_swallows,
        catch_masks,
        dummies,
        AssertionContext::TryBlock,
    );

    if let Some(handler) = &try_stmt.handler {
        if handler.body.body.is_empty() {
            catch_swallows.push(offset_to_line(source, handler.span.start));
        } else {
            let mut catch_assertions = Vec::new();
            for catch_stmt in &handler.body.body {
                scan_statement(
                    catch_stmt,
                    source,
                    &mut catch_assertions,
                    mocks,
                    catch_swallows,
                    catch_masks,
                    dummies,
                    &AssertionContext::CatchBlock,
                );
            }
            // A top-level rethrow lets the try AssertionError propagate, so the
            // catch neither swallows nor masks it.
            let catch_rethrows = handler
                .body
                .body
                .iter()
                .any(|s| matches!(s, Statement::ThrowStatement(_)));
            if catch_assertions.is_empty() && !catch_rethrows {
                catch_swallows.push(offset_to_line(source, handler.span.start));
            }
            // catch-masks: try asserts, catch asserts, catch does not rethrow.
            // The try AssertionError is swallowed and replaced by a passing
            // catch assertion (js-testing-best-practices §1.10).
            if try_has_assertion && !catch_assertions.is_empty() && !catch_rethrows {
                catch_masks.push(offset_to_line(source, handler.span.start));
            }
            assertions.extend(catch_assertions);
        }
    }

    if let Some(finalizer) = &try_stmt.finalizer {
        scan_body(
            &finalizer.body,
            source,
            assertions,
            mocks,
            catch_swallows,
            catch_masks,
            dummies,
            *context,
        );
    }
}

fn collect_dummies_expr(expr: &Expression<'_>, source: &str, out: &mut Vec<DummyLiteral>) {
    match expr {
        Expression::StringLiteral(s) => push_if_dummy(s, source, out),
        Expression::CallExpression(call) => collect_dummies_call(call, source, out),
        Expression::ObjectExpression(obj) => collect_dummies_object(obj, source, out),
        Expression::ArrayExpression(arr) => collect_dummies_array(arr, source, out),
        Expression::StaticMemberExpression(m) => collect_dummies_expr(&m.object, source, out),
        Expression::ComputedMemberExpression(m) => collect_dummies_expr(&m.object, source, out),
        Expression::AwaitExpression(a) => collect_dummies_expr(&a.argument, source, out),
        Expression::ParenthesizedExpression(p) => collect_dummies_expr(&p.expression, source, out),
        _ => {}
    }
}

// Recurse into object property VALUES only. A key like `{ foo: 1 }` names a
// field, not a test input, so flagging it would be a false positive.
fn collect_dummies_object(obj: &ObjectExpression<'_>, source: &str, out: &mut Vec<DummyLiteral>) {
    for prop in &obj.properties {
        match prop {
            ObjectPropertyKind::ObjectProperty(p) => collect_dummies_expr(&p.value, source, out),
            ObjectPropertyKind::SpreadProperty(s) => {
                collect_dummies_expr(&s.argument, source, out);
            }
        }
    }
}

fn collect_dummies_array(arr: &ArrayExpression<'_>, source: &str, out: &mut Vec<DummyLiteral>) {
    for element in &arr.elements {
        match element {
            ArrayExpressionElement::StringLiteral(s) => push_if_dummy(s, source, out),
            ArrayExpressionElement::CallExpression(c) => collect_dummies_call(c, source, out),
            ArrayExpressionElement::ObjectExpression(o) => collect_dummies_object(o, source, out),
            ArrayExpressionElement::ArrayExpression(a) => collect_dummies_array(a, source, out),
            ArrayExpressionElement::SpreadElement(se) => {
                collect_dummies_expr(&se.argument, source, out);
            }
            _ => {}
        }
    }
}

fn collect_dummies_call(call: &CallExpression<'_>, source: &str, out: &mut Vec<DummyLiteral>) {
    // expect(<literal>) is already reported by the tautological rule, so suppress
    // its direct string argument here, including a parenthesized one like
    // expect(("foo")). Nested calls are still recursed into, so
    // expect(slugify("foo")) flags "foo".
    let suppress = matches!(callee_name(&call.callee), Some(("expect", _)));
    collect_dummies_expr(&call.callee, source, out);
    for arg in &call.arguments {
        match arg {
            Argument::StringLiteral(s) => {
                if !suppress {
                    push_if_dummy(s, source, out);
                }
            }
            Argument::CallExpression(c) => collect_dummies_call(c, source, out),
            Argument::ObjectExpression(o) => collect_dummies_object(o, source, out),
            Argument::ArrayExpression(a) => collect_dummies_array(a, source, out),
            Argument::StaticMemberExpression(m) => collect_dummies_expr(&m.object, source, out),
            Argument::ComputedMemberExpression(m) => collect_dummies_expr(&m.object, source, out),
            Argument::AwaitExpression(a) => collect_dummies_expr(&a.argument, source, out),
            Argument::ParenthesizedExpression(p) => {
                if !(suppress && is_direct_string(&p.expression)) {
                    collect_dummies_expr(&p.expression, source, out);
                }
            }
            Argument::SpreadElement(se) => collect_dummies_expr(&se.argument, source, out),
            _ => {}
        }
    }
}

// The expression inside a parenthesized expect argument is a direct string.
// Used to extend expect() suppression to expect(("foo")) so it matches
// expect("foo"). The caller (Argument::ParenthesizedExpression) has already
// unwrapped the outer parentheses.
fn is_direct_string(expr: &Expression<'_>) -> bool {
    matches!(expr, Expression::StringLiteral(_))
}

fn push_if_dummy(s: &StringLiteral<'_>, source: &str, out: &mut Vec<DummyLiteral>) {
    if DUMMY_STRINGS.contains(&s.value.as_str()) {
        out.push(DummyLiteral {
            value: s.value.to_string(),
            line: offset_to_line(source, s.span.start),
        });
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

fn try_assertion(
    call: &CallExpression<'_>,
    source: &str,
    context: &AssertionContext,
) -> Option<Assertion> {
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return None;
    };

    let (target, target_kind, target_root) = find_expect_target(&member.object, source)?;
    let matcher = member.property.name.to_string();
    let is_weak = WEAK_MATCHERS.contains(&matcher.as_str());
    let line = offset_to_line(source, call.span.start);

    Some(Assertion {
        line,
        target,
        target_kind,
        target_root,
        matcher,
        is_weak,
        context: *context,
    })
}

fn find_expect_target(
    expr: &Expression<'_>,
    source: &str,
) -> Option<(String, TargetKind, Option<String>)> {
    match expr {
        Expression::CallExpression(call) => {
            if matches!(callee_name(&call.callee), Some(("expect", _))) {
                let result = call
                    .arguments
                    .first()
                    .map(|arg| {
                        let text = arg.span().source_text(source).to_owned();
                        let kind = classify_argument(arg);
                        let root = arg.as_expression().and_then(expr_root_ident);
                        (text, kind, root)
                    })
                    .unwrap_or_else(|| (String::new(), TargetKind::Other, None));
                Some(result)
            } else {
                None
            }
        }
        Expression::StaticMemberExpression(member) => find_expect_target(&member.object, source),
        _ => None,
    }
}

/// Resolves the root identifier an expression reads from, e.g. `user` in
/// `user.profile.name` or `items[0]`. Returns None for non-reference roots
/// (call results, literals, `this`) where no single source binding applies.
fn expr_root_ident(expr: &Expression<'_>) -> Option<String> {
    match expr {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::StaticMemberExpression(m) => expr_root_ident(&m.object),
        Expression::ComputedMemberExpression(m) => expr_root_ident(&m.object),
        Expression::ParenthesizedExpression(p) => expr_root_ident(&p.expression),
        Expression::TSNonNullExpression(e) => expr_root_ident(&e.expression),
        Expression::TSAsExpression(e) => expr_root_ident(&e.expression),
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
    let kind = mock_kind(call)?;
    let line = offset_to_line(source, call.span.start);
    Some(MockCall { line, kind })
}

fn mock_kind(call: &CallExpression<'_>) -> Option<MockKind> {
    match &call.callee {
        Expression::StaticMemberExpression(member) => {
            let Expression::Identifier(obj) = &member.object else {
                return None;
            };
            if obj.name != "vi" {
                return None;
            }
            match &*member.property.name {
                "fn" => Some(MockKind::ViFn),
                "mock" => Some(MockKind::ViMock),
                "spyOn" => Some(MockKind::ViSpyOn),
                _ => None,
            }
        }
        Expression::Identifier(id) if id.name == "mock" => Some(MockKind::BunMock),
        _ => None,
    }
}

// --- Act (SUT invocation) detection, js-testing-best-practices §1.2 (AAA) ---
//
// A test's "Act" is a call into production code. `body_has_act` returns true if
// the body contains at least one such call. Calls that are part of an `expect`
// chain or a mock setup (`vi.fn`/`vi.mock`/`vi.spyOn`/bare `mock`) are NOT acts;
// every other call (including `expect(sut())`'s inner argument, a `new`, or a
// tagged template) is. The traversal descends every expression position a call
// can hide in, so a test whose only invocation sits in an assignment, ternary,
// object value, or cast is still seen as having an act. Unknown call positions
// bias toward "has act" (no finding), protecting the precision indicator.
fn body_has_act(stmts: &[Statement<'_>]) -> bool {
    stmts.iter().any(stmt_has_act)
}

// Names the body declares locally (its "Arrange"). A test with no local
// binding gets its data from setup hooks or module imports, so a missing Act
// there is not a finding; the binding is what makes "arranged data asserted
// without acting" a real AAA gap. missing-act further requires an assertion to
// target one of these names, so hook-sourced assertion targets do not fire.
fn body_bound_names(stmts: &[Statement<'_>]) -> Vec<String> {
    let mut names = Vec::new();
    collect_bound_names(stmts, &mut names);
    names
}

fn collect_bound_names(stmts: &[Statement<'_>], out: &mut Vec<String>) {
    for stmt in stmts {
        stmt_bound_names(stmt, out);
    }
}

fn stmt_bound_names(stmt: &Statement<'_>, out: &mut Vec<String>) {
    match stmt {
        Statement::VariableDeclaration(vd) => {
            for d in &vd.declarations {
                collect_pattern_names(&d.id, out);
            }
        }
        Statement::BlockStatement(bs) => collect_bound_names(&bs.body, out),
        Statement::IfStatement(s) => {
            stmt_bound_names(&s.consequent, out);
            if let Some(a) = &s.alternate {
                stmt_bound_names(a, out);
            }
        }
        Statement::ForStatement(s) => stmt_bound_names(&s.body, out),
        Statement::ForInStatement(s) => stmt_bound_names(&s.body, out),
        Statement::ForOfStatement(s) => stmt_bound_names(&s.body, out),
        Statement::WhileStatement(s) => stmt_bound_names(&s.body, out),
        Statement::DoWhileStatement(s) => stmt_bound_names(&s.body, out),
        Statement::TryStatement(s) => {
            collect_bound_names(&s.block.body, out);
            if let Some(h) = &s.handler {
                collect_bound_names(&h.body.body, out);
            }
            if let Some(f) = &s.finalizer {
                collect_bound_names(&f.body, out);
            }
        }
        Statement::SwitchStatement(s) => {
            for c in &s.cases {
                collect_bound_names(&c.consequent, out);
            }
        }
        _ => {}
    }
}

// Collects every identifier a binding pattern introduces, descending through
// object/array destructuring and default-value patterns. Unhandled shapes
// yield no name, which only suppresses missing-act (a safe false negative).
fn collect_pattern_names(pat: &BindingPattern<'_>, out: &mut Vec<String>) {
    match pat {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.to_string()),
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_pattern_names(&prop.value, out);
            }
            if let Some(rest) = &obj.rest {
                collect_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_pattern_names(elem, out);
            }
            if let Some(rest) = &arr.rest {
                collect_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(ap) => collect_pattern_names(&ap.left, out),
    }
}

fn stmt_has_act(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::ExpressionStatement(es) => expr_has_act(&es.expression),
        Statement::VariableDeclaration(vd) => vd
            .declarations
            .iter()
            .any(|d| d.init.as_ref().is_some_and(expr_has_act)),
        Statement::ReturnStatement(rs) => rs.argument.as_ref().is_some_and(expr_has_act),
        Statement::BlockStatement(bs) => body_has_act(&bs.body),
        Statement::IfStatement(s) => {
            expr_has_act(&s.test)
                || stmt_has_act(&s.consequent)
                || s.alternate.as_ref().is_some_and(|a| stmt_has_act(a))
        }
        Statement::ForStatement(s) => {
            s.test.as_ref().is_some_and(expr_has_act) || stmt_has_act(&s.body)
        }
        Statement::ForInStatement(s) => expr_has_act(&s.right) || stmt_has_act(&s.body),
        Statement::ForOfStatement(s) => expr_has_act(&s.right) || stmt_has_act(&s.body),
        Statement::WhileStatement(s) => expr_has_act(&s.test) || stmt_has_act(&s.body),
        Statement::DoWhileStatement(s) => expr_has_act(&s.test) || stmt_has_act(&s.body),
        Statement::TryStatement(s) => try_has_act(s),
        Statement::SwitchStatement(s) => {
            expr_has_act(&s.discriminant)
                || s.cases
                    .iter()
                    .any(|c| c.consequent.iter().any(stmt_has_act))
        }
        Statement::ThrowStatement(s) => expr_has_act(&s.argument),
        _ => false,
    }
}

fn try_has_act(try_stmt: &TryStatement<'_>) -> bool {
    body_has_act(&try_stmt.block.body)
        || try_stmt
            .handler
            .as_ref()
            .is_some_and(|h| body_has_act(&h.body.body))
        || try_stmt
            .finalizer
            .as_ref()
            .is_some_and(|f| body_has_act(&f.body))
}

fn expr_has_act(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::CallExpression(call) => call_has_act(call),
        // Constructing production code and tagging a template both invoke code.
        Expression::NewExpression(_) | Expression::TaggedTemplateExpression(_) => true,
        Expression::ParenthesizedExpression(p) => expr_has_act(&p.expression),
        Expression::AwaitExpression(a) => expr_has_act(&a.argument),
        Expression::UnaryExpression(u) => expr_has_act(&u.argument),
        Expression::BinaryExpression(b) => expr_has_act(&b.left) || expr_has_act(&b.right),
        Expression::LogicalExpression(l) => expr_has_act(&l.left) || expr_has_act(&l.right),
        Expression::ConditionalExpression(c) => {
            expr_has_act(&c.test) || expr_has_act(&c.consequent) || expr_has_act(&c.alternate)
        }
        Expression::SequenceExpression(s) => s.expressions.iter().any(expr_has_act),
        Expression::AssignmentExpression(a) => expr_has_act(&a.right),
        Expression::ArrayExpression(arr) => array_has_act(arr),
        Expression::ObjectExpression(obj) => object_has_act(obj),
        Expression::StaticMemberExpression(m) => expr_has_act(&m.object),
        Expression::ComputedMemberExpression(m) => {
            expr_has_act(&m.object) || expr_has_act(&m.expression)
        }
        Expression::TemplateLiteral(t) => t.expressions.iter().any(expr_has_act),
        Expression::TSAsExpression(t) => expr_has_act(&t.expression),
        Expression::TSSatisfiesExpression(t) => expr_has_act(&t.expression),
        Expression::TSNonNullExpression(t) => expr_has_act(&t.expression),
        Expression::TSTypeAssertion(t) => expr_has_act(&t.expression),
        Expression::ChainExpression(c) => chain_has_act(&c.expression),
        _ => false,
    }
}

fn call_has_act(call: &CallExpression<'_>) -> bool {
    if is_act_call(call) {
        return true;
    }
    // An expect/mock call is not itself an act, but a nested call can be: the
    // inner `sut()` in `expect(sut()).toBe(x)` lives in the callee chain/args.
    expr_has_act(&call.callee) || call.arguments.iter().any(arg_has_act)
}

// True when the call invokes production code: neither an `expect(...)` chain nor
// a recognized mock setup.
fn is_act_call(call: &CallExpression<'_>) -> bool {
    !is_assertion_call(call) && mock_kind(call).is_none()
}

fn is_assertion_call(call: &CallExpression<'_>) -> bool {
    matches!(callee_name(&call.callee), Some(("expect", _))) || is_expect_chain(&call.callee)
}

fn is_expect_chain(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::CallExpression(call) => {
            matches!(callee_name(&call.callee), Some(("expect", _)))
        }
        Expression::StaticMemberExpression(m) => is_expect_chain(&m.object),
        _ => false,
    }
}

fn chain_has_act(element: &ChainElement<'_>) -> bool {
    match element {
        ChainElement::CallExpression(call) => call_has_act(call),
        ChainElement::TSNonNullExpression(t) => expr_has_act(&t.expression),
        ChainElement::StaticMemberExpression(m) => expr_has_act(&m.object),
        ChainElement::ComputedMemberExpression(m) => {
            expr_has_act(&m.object) || expr_has_act(&m.expression)
        }
        ChainElement::PrivateFieldExpression(m) => expr_has_act(&m.object),
    }
}

fn arg_has_act(arg: &Argument<'_>) -> bool {
    match arg {
        Argument::SpreadElement(se) => expr_has_act(&se.argument),
        _ => arg.as_expression().is_some_and(expr_has_act),
    }
}

fn array_has_act(arr: &ArrayExpression<'_>) -> bool {
    arr.elements.iter().any(|el| match el {
        ArrayExpressionElement::SpreadElement(se) => expr_has_act(&se.argument),
        ArrayExpressionElement::Elision(_) => false,
        _ => el.as_expression().is_some_and(expr_has_act),
    })
}

fn object_has_act(obj: &ObjectExpression<'_>) -> bool {
    obj.properties.iter().any(|prop| match prop {
        ObjectPropertyKind::ObjectProperty(p) => expr_has_act(&p.value),
        ObjectPropertyKind::SpreadProperty(s) => expr_has_act(&s.argument),
    })
}

fn offset_to_line(source: &str, offset: u32) -> u32 {
    let end = (offset as usize).min(source.len());
    let count = source[..end].bytes().filter(|&b| b == b'\n').count();
    u32::try_from(count).unwrap_or(u32::MAX).saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(source: &str) -> Vec<TestBlock> {
        parse_test_file(source, Path::new("test.tsx")).unwrap()
    }

    // T-025a: byte-scan reports the deepest bracket nesting, mixing kinds and
    // ignoring closers past zero.
    #[test]
    fn max_bracket_depth_counts_deepest_nesting() {
        assert_eq!(max_bracket_depth("[[[a]]]"), 3);
        assert_eq!(max_bracket_depth("f({ a: [1] })"), 3);
        assert_eq!(max_bracket_depth("))]]}"), 0);
        assert_eq!(max_bracket_depth(""), 0);
    }

    // T-025b: a file nested past BRACKET_DEPTH_LIMIT is rejected as a parse
    // error before reaching oxc, so the stack-overflow abort never happens. The
    // guard short-circuits, so this never actually parses the deep input.
    #[test]
    fn rejects_input_deeper_than_bracket_limit() {
        let n = BRACKET_DEPTH_LIMIT + 1;
        let source = format!("const y = {}x{};", "[".repeat(n), "]".repeat(n));
        let err = parse_test_file(&source, Path::new("test.ts")).unwrap_err();
        assert!(err.contains("nesting depth"), "err: {err}");
    }

    // T-025c: depth exactly at the limit is not rejected by the guard (boundary
    // below the trigger). Verified via the pure scan to avoid parsing deep input
    // on the test thread's smaller stack.
    #[test]
    fn depth_at_limit_is_not_over() {
        let n = BRACKET_DEPTH_LIMIT;
        let source = format!("{}x{}", "[".repeat(n), "]".repeat(n));
        assert_eq!(max_bracket_depth(&source), BRACKET_DEPTH_LIMIT);
        assert!(max_bracket_depth(&source) <= BRACKET_DEPTH_LIMIT);
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

    // T-414: external snapshot matcher captured verbatim, so snapshot-external
    // can key on the matcher name. is_weak stays false (a snapshot still asserts).
    #[test]
    fn captures_to_match_snapshot_matcher() {
        let blocks = parse(r#"test("x", () => { expect(x).toMatchSnapshot() })"#);
        assert_eq!(blocks[0].assertions[0].matcher, "toMatchSnapshot");
        assert!(!blocks[0].assertions[0].is_weak);
    }

    // T-415: inline snapshot matcher is a distinct name, so it falls outside the
    // snapshot-external flag set without any special-case handling.
    #[test]
    fn captures_to_match_inline_snapshot_matcher() {
        let blocks = parse(r#"test("x", () => { expect(x).toMatchInlineSnapshot(`1`) })"#);
        assert_eq!(blocks[0].assertions[0].matcher, "toMatchInlineSnapshot");
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
            assert!(blocks[0].assertions[0].is_weak, "{matcher} should be weak");
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
        let source = r#"test("x", async () => { await expect(badCall()).rejects.toThrow("err") })"#;
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
            (
                r#"test("x", () => { expect(true).toBe(true) })"#,
                TargetKind::Literal,
            ),
            (
                r#"test("x", () => { expect(42).toBe(42) })"#,
                TargetKind::Literal,
            ),
            (
                r#"test("x", () => { expect("hello").toEqual("hello") })"#,
                TargetKind::Literal,
            ),
            (
                r#"test("x", () => { expect(null).toBeNull() })"#,
                TargetKind::Literal,
            ),
            (
                r#"test("x", () => { expect(result).toBe(42) })"#,
                TargetKind::Identifier,
            ),
            (
                r#"test("x", () => { expect(fetchUser(1)).toBe(42) })"#,
                TargetKind::CallResult,
            ),
            (
                r#"test("x", () => { expect(obj.prop).toBe(42) })"#,
                TargetKind::Other,
            ),
            (
                r#"test("x", async () => { expect(await fn()).toBe(1) })"#,
                TargetKind::CallResult,
            ),
            (
                r#"test("x", () => { expect((result)).toBe(1) })"#,
                TargetKind::Identifier,
            ),
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
        assert_eq!(
            blocks[0].assertions[0].context,
            AssertionContext::CatchBlock
        );
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

    // T-124: try asserts, catch asserts, no rethrow → catch_masks has entry
    #[test]
    fn catch_masks_try_and_catch_assert() {
        let source = r#"test("x", () => {
            try {
                expect(actual).toBe(expected)
            } catch (e) {
                expect(e).toBeDefined()
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].catch_masks.len(), 1);
    }

    // T-125: try asserts, catch asserts but rethrows → no mask (error propagates)
    #[test]
    fn catch_masks_empty_when_catch_rethrows() {
        let source = r#"test("x", () => {
            try {
                expect(actual).toBe(expected)
            } catch (e) {
                expect(e).toBeDefined()
                throw e
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_masks.is_empty());
    }

    // T-126: try has no assertion, catch asserts → no mask (catch-only-assertion's
    // territory, not a swallowed try assertion)
    #[test]
    fn catch_masks_empty_when_try_has_no_assertion() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                expect(e).toBeDefined()
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_masks.is_empty());
    }

    // T-127: try asserts, catch has no assertion → no mask (catch-swallow's territory)
    #[test]
    fn catch_masks_empty_when_catch_has_no_assertion() {
        let source = r#"test("x", () => {
            try {
                expect(actual).toBe(expected)
            } catch (e) {}
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_masks.is_empty());
    }

    // T-128: sibling try-catch (try-assert/catch-empty + try-empty/catch-assert) →
    // no mask, since neither pair has both a try assertion and a catch assertion
    #[test]
    fn catch_masks_empty_for_sibling_try_catch() {
        let source = r#"test("x", () => {
            try { expect(a).toBe(1) } catch (e) {}
            try { riskyOp() } catch (e) { expect(e).toBeDefined() }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_masks.is_empty());
    }

    // T-129: nested try-catch whose inner catch is empty, wrapped by an outer
    // catch that asserts. The inner try assertion is shielded by the inner
    // catch, so it never reaches the outer catch; the outer try has no top-level
    // assertion → catch_masks empty, and the inner empty catch is catch-swallow.
    #[test]
    fn catch_masks_empty_for_nested_shielded_try() {
        let source = r#"test("x", () => {
            try {
                try {
                    expect(actual).toBe(expected)
                } catch (inner) {}
            } catch (outer) {
                expect(outer).toBeDefined()
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_masks.is_empty());
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }

    fn dummy_values(block: &TestBlock) -> Vec<&str> {
        block
            .dummy_literals
            .iter()
            .map(|d| d.value.as_str())
            .collect()
    }

    // T-220: dummy string as a call argument → collected
    #[test]
    fn dummy_in_call_argument() {
        let blocks = parse(r#"test("creates a user", () => { createUser("foo") })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-221: dummy string as a matcher argument → collected
    #[test]
    fn dummy_in_matcher_argument() {
        let blocks = parse(r#"test("checks the value", () => { expect(x).toBe("bar") })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["bar"]);
    }

    // T-222: dummy nested inside expect's argument call → collected
    #[test]
    fn dummy_nested_in_expect_call() {
        let blocks = parse(r#"test("slugifies input", () => { expect(slugify("foo")).toBe(x) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-223: dummy as expect's direct argument → suppressed (tautological covers it)
    #[test]
    fn dummy_direct_expect_arg_suppressed() {
        let blocks = parse(r#"test("checks literal", () => { expect("foo").toBe(x) })"#);
        assert!(blocks[0].dummy_literals.is_empty());
    }

    // T-224: non-dummy string → not collected
    #[test]
    fn non_dummy_string_ignored() {
        let blocks = parse(r#"test("creates a user", () => { createUser("alice") })"#);
        assert!(blocks[0].dummy_literals.is_empty());
    }

    // T-225: numeric literals → not collected (strings only)
    #[test]
    fn numeric_literals_ignored() {
        let blocks = parse(r#"test("adds numbers", () => { add(123, 1234) })"#);
        assert!(blocks[0].dummy_literals.is_empty());
    }

    // T-226: dummy as the test name → not collected (it is outside the body)
    #[test]
    fn dummy_test_name_ignored() {
        let blocks = parse(r#"test("foo", () => { expect(realResult).toBe(realValue) })"#);
        assert!(blocks[0].dummy_literals.is_empty());
    }

    // T-227: dummy as a variable initializer → collected
    #[test]
    fn dummy_in_variable_init() {
        let blocks =
            parse(r#"test("uses a fixture", () => { const u = "hoge"; expect(f(u)).toBe(1) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["hoge"]);
    }

    // T-228: multiple dummies → all collected with correct lines
    #[test]
    fn multiple_dummies_collected() {
        let source = "test(\"x\", () => {\n  createUser(\"foo\")\n  createOrg(\"bar\")\n})";
        let blocks = parse(source);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo", "bar"]);
        assert_eq!(blocks[0].dummy_literals[0].line, 2);
        assert_eq!(blocks[0].dummy_literals[1].line, 3);
    }

    // T-240: dummy as an object property value → collected
    #[test]
    fn dummy_in_object_value() {
        let blocks = parse(r#"test("creates a user", () => { createUser({ name: "foo" }) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-241: dummy property KEY → not collected (a key names a field, not input)
    #[test]
    fn dummy_object_key_ignored() {
        let blocks = parse(r#"test("creates a user", () => { createUser({ foo: realValue }) })"#);
        assert!(blocks[0].dummy_literals.is_empty());
    }

    // T-242: dummies inside an array literal → all collected
    #[test]
    fn dummies_in_array_literal() {
        let blocks = parse(r#"test("seeds users", () => { seed(["foo", "bar"]) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo", "bar"]);
    }

    // T-243: dummy nested in an array of objects → collected
    #[test]
    fn dummy_in_array_of_objects() {
        let blocks = parse(r#"test("seeds users", () => { seed([{ name: "baz" }]) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["baz"]);
    }

    // T-244: parenthesized direct expect argument → suppressed (parity with T-223)
    #[test]
    fn dummy_parenthesized_expect_arg_suppressed() {
        let blocks = parse(r#"test("checks literal", () => { expect(("foo")).toBe(x) })"#);
        assert!(blocks[0].dummy_literals.is_empty());
    }

    // T-245: spread argument carrying a dummy array → collected
    #[test]
    fn dummy_in_spread_argument() {
        let blocks = parse(r#"test("creates a user", () => { createUser(...["foo"]) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-246: dummy inside a return statement → collected
    #[test]
    fn dummy_in_return_statement() {
        let blocks = parse(r#"test("returns a user", () => { return createUser("foo") })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-247: dummy inside an else branch → collected
    #[test]
    fn dummy_in_else_branch() {
        let blocks =
            parse(r#"test("branches", () => { if (cond) { run() } else { createUser("foo") } })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-248: dummy inside a for loop body → collected
    #[test]
    fn dummy_in_for_body() {
        let blocks =
            parse(r#"test("loops", () => { for (let i = 0; i < 1; i++) { createUser("foo") } })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-249: dummy inside a for-in loop body → collected
    #[test]
    fn dummy_in_for_in_body() {
        let blocks = parse(r#"test("loops", () => { for (const k in o) { createUser("foo") } })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-250: dummy inside a while loop body → collected
    #[test]
    fn dummy_in_while_body() {
        let blocks = parse(r#"test("loops", () => { while (cond) { createUser("foo") } })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-251: dummy inside a do-while loop body → collected
    #[test]
    fn dummy_in_do_while_body() {
        let blocks = parse(r#"test("loops", () => { do { createUser("foo") } while (cond) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-252: dummy inside a switch case → collected
    #[test]
    fn dummy_in_switch_case() {
        let blocks =
            parse(r#"test("switches", () => { switch (x) { case 1: createUser("foo") } })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-253: dummy in an object literal bound to a variable → collected
    #[test]
    fn dummy_in_object_variable_init() {
        let blocks = parse(r#"test("builds a user", () => { const u = { name: "foo" }; use(u) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-254: dummy wrapped in parentheses bound to a variable → collected
    #[test]
    fn dummy_in_parenthesized_variable_init() {
        let blocks = parse(r#"test("builds a value", () => { const u = ("foo"); use(u) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-255: dummy in the object of a computed member access → collected (key not flagged)
    #[test]
    fn dummy_in_computed_member_object() {
        let blocks = parse(r#"test("reads a field", () => { getUser("foo")["id"] })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-256: dummy array spread into an object literal → collected
    #[test]
    fn dummy_in_object_spread() {
        let blocks = parse(r#"test("builds a user", () => { createUser({ ...["foo"] }) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-257: dummy in a call nested inside an array element → collected
    #[test]
    fn dummy_in_array_element_call() {
        let blocks = parse(r#"test("seeds users", () => { seed([makeUser("foo")]) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-258: dummy in a nested array element → collected
    #[test]
    fn dummy_in_nested_array() {
        let blocks = parse(r#"test("seeds users", () => { seed([["foo"]]) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-259: dummy array spread into an array element → collected
    #[test]
    fn dummy_in_array_spread_element() {
        let blocks = parse(r#"test("seeds users", () => { seed([...["foo"]]) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-260: dummy in the object of a computed member passed as an argument → collected
    #[test]
    fn dummy_in_computed_member_argument() {
        let blocks =
            parse(r#"test("reads a field", () => { expect(getUser("foo")["id"]).toBe(1) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-261: non-string array element is skipped while a sibling dummy is collected
    #[test]
    fn non_string_array_element_skipped() {
        let blocks = parse(r#"test("seeds users", () => { seed([1, "foo"]) })"#);
        assert_eq!(dummy_values(&blocks[0]), vec!["foo"]);
    }

    // T-270: arrange-only body (local literal + assertion) has no Act
    #[test]
    fn has_act_false_for_arrange_only() {
        let blocks = parse(r#"test("x", () => { const v = 42; expect(v).toBe(42) })"#);
        assert!(!blocks[0].has_act);
        assert_eq!(blocks[0].bound_names, vec!["v"]);
    }

    // T-271: a bare production call is an Act
    #[test]
    fn has_act_true_for_bare_call() {
        let blocks = parse(r#"test("x", () => { doThing(); expect(x).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-272: a call in a variable initializer is an Act
    #[test]
    fn has_act_true_for_call_in_initializer() {
        let blocks = parse(r#"test("x", () => { const r = compute(2); expect(r).toBe(4) })"#);
        assert!(blocks[0].has_act);
        assert_eq!(blocks[0].bound_names, vec!["r"]);
    }

    // T-273: `new` construction is an Act
    #[test]
    fn has_act_true_for_new_expression() {
        let blocks = parse(r#"test("x", () => { const u = new User(); expect(u.id).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-274: a tagged template invocation is an Act
    #[test]
    fn has_act_true_for_tagged_template() {
        let blocks =
            parse(r#"test("x", () => { const q = sql`SELECT 1`; expect(q).toBeDefined() })"#);
        assert!(blocks[0].has_act);
    }

    // T-275: an awaited call is an Act
    #[test]
    fn has_act_true_for_awaited_call() {
        let blocks = parse(
            r#"test("x", async () => { const r = await fetchUser(1); expect(r).toBeDefined() })"#,
        );
        assert!(blocks[0].has_act);
    }

    // T-276: a call on the right of an assignment is an Act
    #[test]
    fn has_act_true_for_assignment_rhs_call() {
        let blocks = parse(r#"test("x", () => { let r; r = compute(); expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-277: a call inside a ternary branch is an Act
    #[test]
    fn has_act_true_for_ternary_call() {
        let blocks = parse(r#"test("x", () => { const r = cond ? run() : 0; expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-278: a call in an object property value is an Act
    #[test]
    fn has_act_true_for_object_value_call() {
        let blocks =
            parse(r#"test("x", () => { const o = { id: makeId() }; expect(o.id).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-279: a call behind a TS cast is an Act
    #[test]
    fn has_act_true_for_cast_call() {
        let blocks = parse(r#"test("x", () => { const r = run() as number; expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-280: a call inside an optional chain is an Act
    #[test]
    fn has_act_true_for_chain_call() {
        let blocks = parse(r#"test("x", () => { const r = svc?.run(); expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-281: a call in an array element is an Act
    #[test]
    fn has_act_true_for_array_element_call() {
        let blocks = parse(r#"test("x", () => { const xs = [run()]; expect(xs[0]).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-282: an expect/assertion call alone is not an Act
    #[test]
    fn has_act_false_for_assertion_only() {
        let blocks = parse(r#"test("x", () => { expect(result).toBe(1) })"#);
        assert!(!blocks[0].has_act);
        assert!(blocks[0].bound_names.is_empty());
    }

    // T-283: a mock-setup call alone is not an Act
    #[test]
    fn has_act_false_for_mock_setup_only() {
        let blocks = parse(r#"test("x", () => { const m = vi.fn(); expect(m).toBeDefined() })"#);
        assert!(!blocks[0].has_act);
        assert_eq!(blocks[0].bound_names, vec!["m"]);
    }

    // T-284: a local declaration inside a nested block is collected
    #[test]
    fn bound_names_collected_in_nested_block() {
        let blocks = parse(r#"test("x", () => { if (c) { const v = 1; expect(v).toBe(1) } })"#);
        assert_eq!(blocks[0].bound_names, vec!["v"]);
    }

    // T-285: destructuring binds every introduced name
    #[test]
    fn bound_names_from_destructuring() {
        let blocks = parse(r#"test("x", () => { const { a, b } = obj; const [c] = arr })"#);
        assert_eq!(blocks[0].bound_names, vec!["a", "b", "c"]);
    }

    // T-286: assertion target root is the leading identifier of a member chain
    #[test]
    fn target_root_is_member_chain_head() {
        let blocks = parse(r#"test("x", () => { expect(user.profile.name).toBe("a") })"#);
        assert_eq!(blocks[0].assertions[0].target_root.as_deref(), Some("user"));
    }

    // T-287: a literal assertion target has no root identifier
    #[test]
    fn target_root_none_for_literal() {
        let blocks = parse(r#"test("x", () => { expect(42).toBe(42) })"#);
        assert_eq!(blocks[0].assertions[0].target_root, None);
    }

    // T-288: arranged-then-asserted-on-bound-name is the missing-act shape;
    // arranging "expected" while asserting on a hook value is not.
    #[test]
    fn bound_name_matches_asserted_root() {
        let fires = parse(r#"test("x", () => { const total = 42; expect(total).toBe(42) })"#);
        assert!(fires[0].bound_names.iter().any(|n| n == "total"));
        assert_eq!(fires[0].assertions[0].target_root.as_deref(), Some("total"));

        let safe = parse(
            r#"test("x", () => { const expected = "a"; expect(result.name).toBe(expected) })"#,
        );
        assert_eq!(safe[0].assertions[0].target_root.as_deref(), Some("result"));
        assert!(!safe[0].bound_names.iter().any(|n| n == "result"));
    }

    fn parse_ts(source: &str) -> Vec<TestBlock> {
        parse_test_file(source, Path::new("test.ts")).unwrap()
    }

    // T-289: a non-null assertion in the target unwraps to its root identifier
    #[test]
    fn target_root_through_non_null() {
        let blocks = parse(r#"test("x", () => { expect(user!.name).toBe("a") })"#);
        assert_eq!(blocks[0].assertions[0].target_root.as_deref(), Some("user"));
    }

    // T-290: a TS `as` cast in the target unwraps to its root identifier
    #[test]
    fn target_root_through_as_cast() {
        let blocks = parse(r#"test("x", () => { expect((x as Foo).bar).toBe(1) })"#);
        assert_eq!(blocks[0].assertions[0].target_root.as_deref(), Some("x"));
    }

    // T-291: an unrecognized `vi.*` setup call is treated as an Act, not a mock
    #[test]
    fn unknown_vi_call_is_act() {
        let blocks = parse(r#"test("x", () => { vi.useFakeTimers(); expect(x).toBe(1) })"#);
        assert!(blocks[0].has_act);
        assert!(blocks[0].mock_calls.is_empty());
    }

    // T-292: an object-rest binding collects the rest name
    #[test]
    fn bound_names_object_rest() {
        let blocks = parse(r#"test("x", () => { const { a, ...rest } = obj })"#);
        assert_eq!(blocks[0].bound_names, vec!["a", "rest"]);
    }

    // T-293: an array-rest binding collects the rest name
    #[test]
    fn bound_names_array_rest() {
        let blocks = parse(r#"test("x", () => { const [first, ...others] = arr })"#);
        assert_eq!(blocks[0].bound_names, vec!["first", "others"]);
    }

    // T-294: a destructuring default value collects the bound name
    #[test]
    fn bound_names_assignment_pattern() {
        let blocks = parse(r#"test("x", () => { const { a = 1 } = obj })"#);
        assert_eq!(blocks[0].bound_names, vec!["a"]);
    }

    // T-295: a call inside a throw argument is an Act
    #[test]
    fn has_act_true_for_throw_argument() {
        let blocks = parse(r#"test("x", () => { if (cond) throw boom(); expect(x).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-296: an empty statement contributes no Act
    #[test]
    fn has_act_false_for_empty_statement() {
        let blocks = parse(r#"test("x", () => { ; const v = 1; expect(v).toBe(1) })"#);
        assert!(!blocks[0].has_act);
    }

    // T-297: a call behind a unary operator is an Act
    #[test]
    fn has_act_true_for_unary_operand_call() {
        let blocks = parse(r#"test("x", () => { const r = !isReady(); expect(r).toBe(false) })"#);
        assert!(blocks[0].has_act);
    }

    // T-298: a call on a side of a logical expression is an Act
    #[test]
    fn has_act_true_for_logical_operand_call() {
        let blocks = parse(r#"test("x", () => { const r = a || compute(); expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-299: a call inside a sequence expression is an Act
    #[test]
    fn has_act_true_for_sequence_call() {
        let blocks = parse(r#"test("x", () => { const r = (setup(), 42); expect(r).toBe(42) })"#);
        assert!(blocks[0].has_act);
    }

    // T-300: a call interpolated into a template literal is an Act
    #[test]
    fn has_act_true_for_template_interpolation_call() {
        let blocks = parse(r#"test("x", () => { const s = `${makeId()}`; expect(s).toBe("1") })"#);
        assert!(blocks[0].has_act);
    }

    // T-301: a call behind a `satisfies` expression is an Act
    #[test]
    fn has_act_true_for_satisfies_call() {
        let blocks =
            parse(r#"test("x", () => { const r = run() satisfies Foo; expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-302: a call behind a non-null assertion is an Act
    #[test]
    fn has_act_true_for_non_null_call() {
        let blocks = parse(r#"test("x", () => { const r = run()!; expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-303: a call behind an angle-bracket type assertion is an Act
    #[test]
    fn has_act_true_for_type_assertion_call() {
        let blocks = parse_ts(r#"test("x", () => { const r = <number>run(); expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-304: a call on a member chain head reached via optional chaining is an Act
    #[test]
    fn has_act_true_for_optional_static_member_call() {
        let blocks = parse(r#"test("x", () => { const r = make()?.prop; expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-305: a call on a computed-member chain head reached via optional chaining is an Act
    #[test]
    fn has_act_true_for_optional_computed_member_call() {
        let blocks = parse(r#"test("x", () => { const r = arr()?.[0]; expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-306: a call inside a spread argument is an Act
    #[test]
    fn has_act_true_for_spread_argument_call() {
        let blocks = parse(r#"test("x", () => { const r = wrap(...gen()); expect(r).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-307: a call inside an array spread element is an Act
    #[test]
    fn has_act_true_for_array_spread_call() {
        let blocks = parse(r#"test("x", () => { const xs = [...gen()]; expect(xs[0]).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-308: an array elision is skipped while a sibling call is still an Act
    #[test]
    fn has_act_true_with_array_elision() {
        let blocks = parse(r#"test("x", () => { const xs = [, run()]; expect(xs[1]).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-309: a call inside an object spread property is an Act
    #[test]
    fn has_act_true_for_object_spread_call() {
        let blocks = parse(r#"test("x", () => { const o = { ...gen() }; expect(o.a).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }
}
