use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpression, ArrayExpressionElement, BindingPattern, CallExpression,
    ChainElement, Expression, FunctionBody, ObjectExpression, ObjectPropertyKind, Statement,
    StringLiteral, TryStatement,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use std::mem;
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

// oxc's recursive-descent parser overflows the native stack on pathologically
// nested input, aborting with SIGABRT — which `catch_unwind` cannot intercept,
// so one deep file would take down analysis of every sibling and violate the
// ADR-0066 fault-isolation contract. Analysis runs on a large stack (main.rs
// `ANALYZER_STACK_SIZE`, 256 MiB) to push the overflow floor far above any
// realistic input, but the two recursion shapes need different handling:
//
//   - Bracket nesting (`[[…]]`, `((…))`, `{a:{…}}`, `f(f(…))`) is detectable
//     pre-parse by a byte scan, because every opener has a matching closer.
//     The guard below rejects an over-deep file as a parse error, giving
//     brackets an absolute no-SIGABRT guarantee independent of stack size.
//   - Right-associative recursion (ternary alternate spine, assignment `=`,
//     exponent `**`, prefix-unary) has no closing token, so byte-counting
//     cannot measure its depth: `a=b=c` recurses 3 deep in 3 bytes while the
//     left-associative `a===b===c` recurses 0 deep in 6 bytes — byte count is
//     anti-correlated with depth (issue #56). It cannot be guarded pre-parse;
//     the large stack alone bounds it, raising its floor to ~250k levels.
//     Depth past that still SIGABRTs — accepted as unreachable for authored or
//     transpiled sources, not provably impossible (generated code is the edge).
//
// On the 256 MiB stack the bracket overflow floor is ~86k levels (~3KB/level),
// so 500 sits ~170x below it and ~10x above any realistic source nesting.
const BRACKET_DEPTH_LIMIT: usize = 500;

// Maximum `{`/`[`/`(` nesting depth in `source`, counted byte-wise without
// lexing. String, comment, and regex contents inflate the count (an unmatched
// brace in a string literal is counted), but the wide margin between the limit
// and both realistic depth (~tens) and the in-thread overflow floor (~86k)
// absorbs that false-positive risk. This catches only bracket nesting;
// right-associative recursion carries no bracket to count and is bounded by the
// analyzer stack size instead (`ANALYZER_STACK_SIZE` in main.rs).
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
    let src = Source::new(source);
    walk_statements(&ret.program.body, &src, &mut blocks);
    Ok(blocks)
}

fn walk_statements(stmts: &[Statement<'_>], src: &Source, blocks: &mut Vec<TestBlock>) {
    for stmt in stmts {
        if let Statement::ExpressionStatement(expr_stmt) = stmt {
            check_test_call(&expr_stmt.expression, src, blocks);
        }
    }
}

fn check_test_call(expr: &Expression<'_>, src: &Source, blocks: &mut Vec<TestBlock>) {
    let Expression::CallExpression(call) = expr else {
        return;
    };

    let (name, modifier) = match callee_name(&call.callee) {
        Some(pair) => pair,
        None => return,
    };

    match name {
        "test" | "it" => {
            if let Some(mut block) = extract_test_block(call, src) {
                block.modifier = modifier;
                blocks.push(block);
            } else if modifier == Some(TestModifier::Todo) {
                // test.todo("x") has no callback — create minimal block
                if let Some(name) = first_string_arg(&call.arguments) {
                    let line = src.line(call.span.start);
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
                walk_statements(&body.statements, src, blocks);
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
        // `it.each(table)(name, fn)` / `describe.each(...)(...)`: the callee is
        // itself a call to `<it|test|describe>.each(...)`, so the outer call is
        // the test/suite and its (name, fn) arguments drive extraction (#88).
        // Template-literal `it.each` `` `...` `` is a tagged template, not a
        // call, so it stays unhandled here (its name is also non-literal).
        // Modifier-combined forms (`it.only.each`, `it.skip.each`) have a
        // member-expression object, not a bare identifier, so they fall through
        // and stay invisible — the same as before #88; closing them is left to a
        // follow-up since the modifier would also need to thread through.
        Expression::CallExpression(inner) => {
            let Expression::StaticMemberExpression(member) = &inner.callee else {
                return None;
            };
            if &*member.property.name != "each" {
                return None;
            }
            match &member.object {
                Expression::Identifier(id) if matches!(&*id.name, "it" | "test" | "describe") => {
                    Some((&id.name, None))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn extract_test_block(call: &CallExpression<'_>, src: &Source) -> Option<TestBlock> {
    let name = first_string_arg(&call.arguments)?;
    let body = callback_body(&call.arguments)?;
    let line = src.line(call.span.start);
    let has_empty_body = body.statements.is_empty();
    let has_act = body_has_act(&body.statements);
    let bound_names = body_bound_names(&body.statements);

    let mut collector = Collector::new(src);
    collector.scan_body(&body.statements, AssertionContext::TopLevel);
    let Collector {
        assertions,
        mocks: mock_calls,
        catch_swallows,
        catch_masks,
        dummies: dummy_literals,
        ..
    } = collector;

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

// The test callback is the first function-expression argument. Scanning by
// shape rather than a fixed index also reaches vitest's 3-arg
// `test(name, options, fn)` form, whose callback sits at index 2 (#88).
fn callback_body<'a>(args: &'a [Argument<'a>]) -> Option<&'a FunctionBody<'a>> {
    args.iter().find_map(arg_function_body)
}

fn arg_function_body<'a>(arg: &'a Argument<'a>) -> Option<&'a FunctionBody<'a>> {
    match arg {
        Argument::ArrowFunctionExpression(arrow) => Some(&arrow.body),
        Argument::FunctionExpression(func) => func.body.as_deref(),
        _ => None,
    }
}

// Gathers the five output sinks (assertions / mocks / catch_swallows /
// catch_masks / dummies) for one test block so the scan walk threads a single
// `&mut self` instead of one parameter per sink. Adding a sink is now a field,
// not a signature change across every recursive arm.
struct Collector<'s, 'a> {
    src: &'s Source<'a>,
    assertions: Vec<Assertion>,
    mocks: Vec<MockCall>,
    catch_swallows: Vec<u32>,
    catch_masks: Vec<u32>,
    dummies: Vec<DummyLiteral>,
}

impl<'s, 'a> Collector<'s, 'a> {
    fn new(src: &'s Source<'a>) -> Self {
        Self {
            src,
            assertions: Vec::new(),
            mocks: Vec::new(),
            catch_swallows: Vec::new(),
            catch_masks: Vec::new(),
            dummies: Vec::new(),
        }
    }

    fn scan_body(&mut self, stmts: &[Statement<'_>], context: AssertionContext) {
        for stmt in stmts {
            self.scan_statement(stmt, &context);
        }
    }

    fn scan_statement(&mut self, stmt: &Statement<'_>, context: &AssertionContext) {
        match stmt {
            Statement::ExpressionStatement(es) => {
                self.scan_expr(&es.expression, context);
                collect_dummies_expr(&es.expression, self.src, &mut self.dummies);
            }
            Statement::VariableDeclaration(vd) => {
                for decl in &vd.declarations {
                    if let Some(init) = &decl.init {
                        self.scan_expr(init, context);
                        collect_dummies_expr(init, self.src, &mut self.dummies);
                    }
                }
            }
            Statement::ReturnStatement(rs) => {
                if let Some(arg) = &rs.argument {
                    self.scan_expr(arg, context);
                    collect_dummies_expr(arg, self.src, &mut self.dummies);
                }
            }
            Statement::BlockStatement(bs) => {
                self.scan_body(&bs.body, *context);
            }
            Statement::IfStatement(if_stmt) => {
                self.scan_statement(&if_stmt.consequent, &AssertionContext::IfBranch);
                if let Some(alt) = &if_stmt.alternate {
                    self.scan_statement(alt, &AssertionContext::IfBranch);
                }
            }
            Statement::ForStatement(for_stmt) => {
                self.scan_statement(&for_stmt.body, context);
            }
            Statement::ForInStatement(for_in) => {
                self.scan_statement(&for_in.body, context);
            }
            Statement::ForOfStatement(for_of) => {
                self.scan_statement(&for_of.body, context);
            }
            Statement::WhileStatement(while_stmt) => {
                self.scan_statement(&while_stmt.body, context);
            }
            Statement::DoWhileStatement(do_while) => {
                self.scan_statement(&do_while.body, context);
            }
            Statement::TryStatement(try_stmt) => {
                self.scan_try_statement(try_stmt, context);
            }
            Statement::SwitchStatement(switch_stmt) => {
                for case in &switch_stmt.cases {
                    self.scan_body(&case.consequent, *context);
                }
            }
            _ => {}
        }
    }

    fn scan_try_statement(&mut self, try_stmt: &TryStatement<'_>, context: &AssertionContext) {
        // catch-masks judges only top-level try assertions, mirroring the
        // top-level rethrow check on the catch. An assertion inside a nested
        // try-catch is shielded by that inner catch and never reaches this
        // catch, so a delta over the body-wide flattened vec (which scan_body
        // bubbles inner assertions into) would misfire on the outer catch.
        let try_has_assertion = try_block_has_top_level_assertion(&try_stmt.block.body, self.src);
        self.scan_body(&try_stmt.block.body, AssertionContext::TryBlock);

        if let Some(handler) = &try_stmt.handler {
            if handler.body.body.is_empty() {
                self.catch_swallows.push(self.src.line(handler.span.start));
            } else {
                // Divert assertions to a catch-local vec while keeping mocks /
                // swallows / masks / dummies shared: the swallow / mask checks
                // below read only the catch's own assertions. mem::take leaves
                // self.assertions empty to collect them, then mem::replace
                // restores the shared vec and hands back the catch-local one.
                let saved = mem::take(&mut self.assertions);
                for catch_stmt in &handler.body.body {
                    self.scan_statement(catch_stmt, &AssertionContext::CatchBlock);
                }
                let catch_assertions = mem::replace(&mut self.assertions, saved);
                // A rethrow anywhere in the catch — including nested in if / for
                // / block / switch / try — lets the try AssertionError
                // propagate, so the catch neither swallows nor masks it (#27).
                let catch_rethrows = body_contains_throw(&handler.body.body);
                if catch_assertions.is_empty() && !catch_rethrows {
                    self.catch_swallows.push(self.src.line(handler.span.start));
                }
                // catch-masks: try asserts, catch asserts, catch does not
                // rethrow. The try AssertionError is swallowed and replaced by a
                // passing catch assertion (js-testing-best-practices §1.10).
                if try_has_assertion && !catch_assertions.is_empty() && !catch_rethrows {
                    self.catch_masks.push(self.src.line(handler.span.start));
                }
                self.assertions.extend(catch_assertions);
            }
        }

        if let Some(finalizer) = &try_stmt.finalizer {
            self.scan_body(&finalizer.body, *context);
        }
    }
}

// True when an assertion appears as a direct (top-level) statement of the try
// block. Used by catch-masks to decide whether the try contributes an
// AssertionError the catch could swallow. Nested control flow is intentionally
// excluded for consistency with the top-level catch rethrow check.
fn try_block_has_top_level_assertion(body: &[Statement<'_>], src: &Source) -> bool {
    body.iter().any(|stmt| {
        let Statement::ExpressionStatement(es) = stmt else {
            return false;
        };
        let mut probe = Collector::new(src);
        probe.scan_expr(&es.expression, &AssertionContext::TryBlock);
        !probe.assertions.is_empty()
    })
}

// Whether any statement rethrows. Walks control-flow nesting (block / if / for
// / while / switch / try) but stops at nested function expressions: a throw
// inside a callback (e.g. `items.forEach(() => { throw e })`) does not rethrow
// synchronously from the catch. Recursion depth is bounded by the pre-parse
// BRACKET_DEPTH_LIMIT guard (#25), which caps AST nesting before any scan runs.
fn body_contains_throw(stmts: &[Statement<'_>]) -> bool {
    stmts.iter().any(stmt_contains_throw)
}

fn stmt_contains_throw(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::ThrowStatement(_) => true,
        Statement::BlockStatement(bs) => body_contains_throw(&bs.body),
        Statement::IfStatement(s) => {
            stmt_contains_throw(&s.consequent)
                || s.alternate.as_ref().is_some_and(|a| stmt_contains_throw(a))
        }
        Statement::ForStatement(s) => stmt_contains_throw(&s.body),
        Statement::ForInStatement(s) => stmt_contains_throw(&s.body),
        Statement::ForOfStatement(s) => stmt_contains_throw(&s.body),
        Statement::WhileStatement(s) => stmt_contains_throw(&s.body),
        Statement::DoWhileStatement(s) => stmt_contains_throw(&s.body),
        Statement::TryStatement(s) => {
            body_contains_throw(&s.block.body)
                || s.handler
                    .as_ref()
                    .is_some_and(|h| body_contains_throw(&h.body.body))
                || s.finalizer
                    .as_ref()
                    .is_some_and(|f| body_contains_throw(&f.body))
        }
        Statement::SwitchStatement(s) => s
            .cases
            .iter()
            .any(|c| c.consequent.iter().any(stmt_contains_throw)),
        _ => false,
    }
}

fn collect_dummies_expr(expr: &Expression<'_>, src: &Source, out: &mut Vec<DummyLiteral>) {
    match expr {
        Expression::StringLiteral(s) => push_if_dummy(s, src, out),
        Expression::CallExpression(call) => collect_dummies_call(call, src, out),
        Expression::ObjectExpression(obj) => collect_dummies_object(obj, src, out),
        Expression::ArrayExpression(arr) => collect_dummies_array(arr, src, out),
        Expression::StaticMemberExpression(m) => collect_dummies_expr(&m.object, src, out),
        Expression::ComputedMemberExpression(m) => collect_dummies_expr(&m.object, src, out),
        Expression::AwaitExpression(a) => collect_dummies_expr(&a.argument, src, out),
        Expression::ParenthesizedExpression(p) => collect_dummies_expr(&p.expression, src, out),
        _ => {}
    }
}

// Recurse into object property VALUES only. A key like `{ foo: 1 }` names a
// field, not a test input, so flagging it would be a false positive.
fn collect_dummies_object(obj: &ObjectExpression<'_>, src: &Source, out: &mut Vec<DummyLiteral>) {
    for prop in &obj.properties {
        match prop {
            ObjectPropertyKind::ObjectProperty(p) => collect_dummies_expr(&p.value, src, out),
            ObjectPropertyKind::SpreadProperty(s) => {
                collect_dummies_expr(&s.argument, src, out);
            }
        }
    }
}

fn collect_dummies_array(arr: &ArrayExpression<'_>, src: &Source, out: &mut Vec<DummyLiteral>) {
    for element in &arr.elements {
        match element {
            ArrayExpressionElement::StringLiteral(s) => push_if_dummy(s, src, out),
            ArrayExpressionElement::CallExpression(c) => collect_dummies_call(c, src, out),
            ArrayExpressionElement::ObjectExpression(o) => collect_dummies_object(o, src, out),
            ArrayExpressionElement::ArrayExpression(a) => collect_dummies_array(a, src, out),
            ArrayExpressionElement::SpreadElement(se) => {
                collect_dummies_expr(&se.argument, src, out);
            }
            _ => {}
        }
    }
}

fn collect_dummies_call(call: &CallExpression<'_>, src: &Source, out: &mut Vec<DummyLiteral>) {
    // expect(<literal>) is already reported by the tautological rule, so suppress
    // its direct string argument here, including a parenthesized one like
    // expect(("foo")). Nested calls are still recursed into, so
    // expect(slugify("foo")) flags "foo".
    let suppress = matches!(callee_name(&call.callee), Some(("expect", _)));
    collect_dummies_expr(&call.callee, src, out);
    for arg in &call.arguments {
        match arg {
            Argument::StringLiteral(s) => {
                if !suppress {
                    push_if_dummy(s, src, out);
                }
            }
            Argument::CallExpression(c) => collect_dummies_call(c, src, out),
            Argument::ObjectExpression(o) => collect_dummies_object(o, src, out),
            Argument::ArrayExpression(a) => collect_dummies_array(a, src, out),
            Argument::StaticMemberExpression(m) => collect_dummies_expr(&m.object, src, out),
            Argument::ComputedMemberExpression(m) => collect_dummies_expr(&m.object, src, out),
            Argument::AwaitExpression(a) => collect_dummies_expr(&a.argument, src, out),
            Argument::ParenthesizedExpression(p) => {
                if !(suppress && is_direct_string(&p.expression)) {
                    collect_dummies_expr(&p.expression, src, out);
                }
            }
            Argument::SpreadElement(se) => collect_dummies_expr(&se.argument, src, out),
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

fn push_if_dummy(s: &StringLiteral<'_>, src: &Source, out: &mut Vec<DummyLiteral>) {
    if DUMMY_STRINGS.contains(&s.value.as_str()) {
        out.push(DummyLiteral {
            value: s.value.to_string(),
            line: src.line(s.span.start),
        });
    }
}

impl Collector<'_, '_> {
    fn scan_expr(&mut self, expr: &Expression<'_>, context: &AssertionContext) {
        // Iterative worklist instead of recursion. Logical / conditional / member
        // / callee chains are bracket-free, so the file-level bracket guard (#25)
        // cannot bound their depth; a recursive walk overflows the stack on
        // pathological input (tens of thousands of chained `&&`, verified). A heap
        // stack removes the failure mode at any depth. Children are pushed in
        // reverse so they pop in source order, preserving assertion indices (left
        // before right in `a && b`, test before branches in a ternary).
        let mut stack: Vec<(&Expression<'_>, AssertionContext)> = vec![(expr, *context)];
        while let Some((expr, context)) = stack.pop() {
            match expr {
                Expression::CallExpression(call) => {
                    if let Some(a) = try_assertion(call, self.src, &context) {
                        self.assertions.push(a);
                    } else if let Some(m) = try_mock(call, self.src) {
                        self.mocks.push(m);
                    } else {
                        // A non-assertion/mock call may invoke callback arguments
                        // inline (forEach / map / waitFor / `it.each` body), so the
                        // assertions and mocks inside them are real. Descend into
                        // each function-expression argument body via the full body
                        // walk so nested catch-swallows and dummies are seen too
                        // (#88). A function assigned to a variable or returned is a
                        // definition, not an argument, so it stays un-descended and
                        // weak-assertion still fires on a test that only defines it.
                        // The body is walked eagerly here rather than via the
                        // worklist, so a callback's assertions are ordered at this
                        // descent point, not strictly by source position across a
                        // `foo(cb).bar(cb)` chain; every rule reads assertions
                        // order-independently, so only display order is affected.
                        for arg in &call.arguments {
                            if let Some(body) = arg_function_body(arg) {
                                self.scan_body(&body.statements, context);
                            }
                        }
                        // Chained calls like vi.fn().mockReturnValue()
                        stack.push((&call.callee, context));
                    }
                }
                Expression::StaticMemberExpression(member) => {
                    stack.push((&member.object, context));
                }
                Expression::AwaitExpression(ae) => {
                    stack.push((&ae.argument, context));
                }
                Expression::LogicalExpression(logical) => {
                    // Left operand runs unconditionally (keeps the parent context);
                    // the right operand runs only when the operator short-circuits
                    // through, so it is guarded like an if-branch assertion
                    // (e.g. `cond && expect(x).toBe(1)`).
                    stack.push((&logical.right, AssertionContext::IfBranch));
                    stack.push((&logical.left, context));
                }
                Expression::ConditionalExpression(conditional) => {
                    // The test runs unconditionally; both branches are guarded.
                    stack.push((&conditional.alternate, AssertionContext::IfBranch));
                    stack.push((&conditional.consequent, AssertionContext::IfBranch));
                    stack.push((&conditional.test, context));
                }
                Expression::ParenthesizedExpression(paren) => {
                    stack.push((&paren.expression, context));
                }
                Expression::SequenceExpression(seq) => {
                    // Every element of `(a, b, c)` evaluates unconditionally, so
                    // each keeps the parent context. Push in reverse for source
                    // order (#31).
                    for sub in seq.expressions.iter().rev() {
                        stack.push((sub, context));
                    }
                }
                // Transparent type wrappers around an assertion call, e.g.
                // `(expect(x).toBe(1) as any)`; descend to the inner call (#31).
                Expression::TSAsExpression(e) => stack.push((&e.expression, context)),
                Expression::TSSatisfiesExpression(e) => stack.push((&e.expression, context)),
                Expression::TSNonNullExpression(e) => stack.push((&e.expression, context)),
                Expression::TSTypeAssertion(e) => stack.push((&e.expression, context)),
                _ => {}
            }
        }
    }
}

fn try_assertion(
    call: &CallExpression<'_>,
    src: &Source,
    context: &AssertionContext,
) -> Option<Assertion> {
    if let Some(assertion) = try_node_assert_call(call, src, context) {
        return Some(assertion);
    }

    let Expression::StaticMemberExpression(member) = &call.callee else {
        return None;
    };

    let expect_call = find_expect_call(&member.object)?;
    let arg = expect_call.arguments.first();
    let (target, target_kind, target_root) = target_from_arg(arg, src);
    let matcher = member.property.name.to_string();
    // A weak matcher (toBeTruthy/toBeDefined/toBeFalsy) is normally weak, but a
    // throwing Testing Library query (getBy*/findBy*) in `expect(...)` already
    // guarantees the element exists, so the assertion self-verifies (#91).
    let is_weak = WEAK_MATCHERS.contains(&matcher.as_str())
        && !arg
            .and_then(|arg| arg.as_expression())
            .and_then(arg_query_name)
            .is_some_and(is_throwing_query_name);
    let line = src.line(call.span.start);

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

/// node:assert recognition: `assert.equal(a, b)` (member form, callee shape
/// mirrors `mock_kind`'s `obj.method` check) and bare `assert(x)` (call form).
/// `assert(x)`/`assert.ok(x)` verify truthiness only, and `assert.fail([message])`
/// throws unconditionally without comparing anything (its argument is a failure
/// message, not a target), so all three are weak; the other documented
/// comparison methods (equal/strictEqual/deepEqual/deepStrictEqual/
/// notEqual/notStrictEqual/notDeepEqual/notDeepStrictEqual/match/throws/
/// rejects) compare concrete values or control flow, so are strong. See
/// https://nodejs.org/docs/latest/api/assert.html for the method list.
fn try_node_assert_call(
    call: &CallExpression<'_>,
    src: &Source,
    context: &AssertionContext,
) -> Option<Assertion> {
    if !is_node_assert_call(call) {
        return None;
    }
    let (matcher, is_weak) = match &call.callee {
        Expression::StaticMemberExpression(member) => {
            let name = member.property.name.to_string();
            // `assert.ok(x)` verifies truthiness only, so is weak. `assert.fail([message])`
            // takes a failure message (not a value under test) as its first argument and
            // throws unconditionally without comparing anything, so it is weak too (#98).
            let is_weak = name == "ok" || name == "fail";
            (name, is_weak)
        }
        Expression::Identifier(_) => ("assert".to_owned(), true),
        _ => return None,
    };

    // `assert.fail(message)`'s first argument is a failure message, not an
    // asserted value/target, so extracting it as the target would misreport
    // the message text as a real comparison target (#98).
    let arg = if matcher == "fail" {
        None
    } else {
        call.arguments.first()
    };
    let (target, target_kind, target_root) = target_from_arg(arg, src);
    let line = src.line(call.span.start);

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

fn find_expect_call<'a>(expr: &'a Expression<'a>) -> Option<&'a CallExpression<'a>> {
    match expr {
        Expression::CallExpression(call) => {
            matches!(callee_name(&call.callee), Some(("expect", _))).then_some(call)
        }
        Expression::StaticMemberExpression(member) => find_expect_call(&member.object),
        // Transparent wrappers between `.matcher` and the `expect(...)` call
        // (mirrors expr_root_ident / expr_has_act): `(expect(v)).toBe`,
        // `(expect(v) as T).toBe`, `expect(v)!.toBe` all assert on expect (#31).
        Expression::ParenthesizedExpression(p) => find_expect_call(&p.expression),
        Expression::TSAsExpression(e) => find_expect_call(&e.expression),
        Expression::TSSatisfiesExpression(e) => find_expect_call(&e.expression),
        Expression::TSNonNullExpression(e) => find_expect_call(&e.expression),
        Expression::TSTypeAssertion(e) => find_expect_call(&e.expression),
        _ => None,
    }
}

/// The query function name an `expect()` argument calls, e.g. `getByText` in
/// `expect(screen.getByText("ok"))` or `expect(getByText("ok"))`. `findBy*` is
/// async, so an `await` wrapper is unwrapped (`expect(await findByText(...))`).
/// Returns None when the argument is not a (member or bare) call (#91).
fn arg_query_name<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr {
        Expression::CallExpression(call) => match &call.callee {
            Expression::Identifier(id) => Some(&id.name),
            Expression::StaticMemberExpression(m) => Some(&m.property.name),
            _ => None,
        },
        Expression::AwaitExpression(e) => arg_query_name(&e.argument),
        _ => None,
    }
}

/// Testing Library queries that throw when no element matches, so an element
/// they return is already proven to exist — `expect(getByText(...)).toBeTruthy()`
/// self-verifies and is not weak. `queryBy*`/`queryAllBy*` return null instead
/// (no throw), so they stay weak (#91).
fn is_throwing_query_name(name: &str) -> bool {
    ["getBy", "getAllBy", "findBy", "findAllBy"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
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
        Expression::TSSatisfiesExpression(e) => expr_root_ident(&e.expression),
        Expression::TSTypeAssertion(e) => expr_root_ident(&e.expression),
        _ => None,
    }
}

/// Builds the (target text, target kind, target root identifier) triple an
/// assertion records from its first argument, shared by `try_assertion`
/// (`expect(...)` matchers) and `try_node_assert_call` (`assert.*`).
fn target_from_arg(
    arg: Option<&Argument<'_>>,
    src: &Source,
) -> (String, TargetKind, Option<String>) {
    match arg {
        Some(arg) => {
            let text = arg.span().source_text(src.text).to_owned();
            let kind = classify_argument(arg);
            let root = arg.as_expression().and_then(expr_root_ident);
            (text, kind, root)
        }
        None => (String::new(), TargetKind::Other, None),
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

fn try_mock(call: &CallExpression<'_>, src: &Source) -> Option<MockCall> {
    let kind = mock_kind(call)?;
    let line = src.line(call.span.start);
    Some(MockCall { line, kind })
}

fn mock_kind(call: &CallExpression<'_>) -> Option<MockKind> {
    match &call.callee {
        Expression::StaticMemberExpression(member) => {
            let Expression::Identifier(obj) = &member.object else {
                return None;
            };
            // `vi.*` (vitest) and `jest.*` share the same mock API surface, so
            // both must feed mock-overuse for symmetric coverage (#89 B3).
            // Before this, mock-overuse never fired on Jest suites while
            // mock-only (matcher based) did, leaving coverage asymmetric within
            // one tool. Variant names keep the `Vi` prefix as an internal tag.
            if obj.name != "vi" && obj.name != "jest" {
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
// chain or a mock setup (`vi.*`/`jest.*` fn|mock|spyOn / bare `mock`) are NOT acts;
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

// A binding pattern can hide a production call in two spots a name-collector skips:
// a destructuring default (`const { a = run() } = obj`, on `AssignmentPattern::right`)
// and a computed key (`const { [run()]: a } = obj`, on a computed `BindingProperty::key`).
// `collect_pattern_names` skips both since it only gathers names; missing either leaves
// the only Act in the declaration invisible and missing-act fires a false positive.
fn pattern_has_act(pat: &BindingPattern<'_>) -> bool {
    match pat {
        BindingPattern::BindingIdentifier(_) => false,
        BindingPattern::ObjectPattern(obj) => {
            obj.properties.iter().any(|prop| {
                (prop.computed && prop.key.as_expression().is_some_and(expr_has_act))
                    || pattern_has_act(&prop.value)
            }) || obj
                .rest
                .as_ref()
                .is_some_and(|rest| pattern_has_act(&rest.argument))
        }
        BindingPattern::ArrayPattern(arr) => {
            arr.elements.iter().flatten().any(pattern_has_act)
                || arr
                    .rest
                    .as_ref()
                    .is_some_and(|rest| pattern_has_act(&rest.argument))
        }
        BindingPattern::AssignmentPattern(ap) => {
            expr_has_act(&ap.right) || pattern_has_act(&ap.left)
        }
    }
}

fn stmt_has_act(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::ExpressionStatement(es) => expr_has_act(&es.expression),
        Statement::VariableDeclaration(vd) => vd
            .declarations
            .iter()
            .any(|d| d.init.as_ref().is_some_and(expr_has_act) || pattern_has_act(&d.id)),
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
    matches!(callee_name(&call.callee), Some(("expect", _)))
        || is_expect_chain(&call.callee)
        || is_node_assert_call(call)
}

/// Callee shape recognition shared by `is_assertion_call` (Act/assertion
/// classification, #92 U-002) and `try_node_assert_call` (assertion
/// content extraction): `assert.equal(a, b)` (member form) or bare
/// `assert(x)` (call form). See `try_node_assert_call` for the method list.
fn is_node_assert_call(call: &CallExpression<'_>) -> bool {
    match &call.callee {
        Expression::StaticMemberExpression(member) => {
            matches!(&member.object, Expression::Identifier(obj) if obj.name == "assert")
        }
        Expression::Identifier(id) => id.name == "assert",
        _ => false,
    }
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

/// The source text plus a precomputed ascending list of every `'\n'` byte
/// offset, built once per file. Line lookups are then O(log n) instead of
/// re-scanning from the start each call (the old per-call `offset_to_line`
/// made the whole walk O(n²); #28). Threaded through the walk so both the
/// raw text (span extraction) and line numbers share one allocation.
struct Source<'a> {
    text: &'a str,
    newline_offsets: Vec<u32>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str) -> Self {
        let newline_offsets = text
            .bytes()
            .enumerate()
            .filter(|&(_, b)| b == b'\n')
            .map(|(i, _)| u32::try_from(i).unwrap_or(u32::MAX))
            .collect();
        Self {
            text,
            newline_offsets,
        }
    }

    /// 1-based line of `offset`. Counts newlines strictly before `offset`,
    /// matching the old `source[..offset.min(len)]` scan: a `'\n'` exactly at
    /// `offset` is not counted.
    fn line(&self, offset: u32) -> u32 {
        let count = self.newline_offsets.partition_point(|&p| p < offset);
        u32::try_from(count).unwrap_or(u32::MAX).saturating_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{check_missing_act, check_weak_assertions};
    use std::path::Path;

    fn parse(source: &str) -> Vec<TestBlock> {
        parse_test_file(source, Path::new("test.tsx")).unwrap()
    }

    // The original O(n) line lookup, kept verbatim so the characterization test
    // below can assert `Source::line` stays byte-identical to it (#28).
    fn naive_offset_to_line(source: &str, offset: u32) -> u32 {
        let end = (offset as usize).min(source.len());
        let count = source[..end].bytes().filter(|&b| b == b'\n').count();
        u32::try_from(count).unwrap_or(u32::MAX).saturating_add(1)
    }

    // T-001..T-006: Source::line pins the documented boundary behaviour.
    #[test]
    fn source_line_handles_boundaries() {
        // empty file → line 1 at offset 0
        assert_eq!(Source::new("").line(0), 1);
        let s = "a\nb\nc";
        assert_eq!(Source::new(s).line(0), 1); // first char
        assert_eq!(Source::new(s).line(2), 2); // 'b', after first '\n'
        assert_eq!(Source::new(s).line(1), 1); // exactly on '\n' is not counted
        // offset past len clamps to counting every newline
        assert_eq!(Source::new("a\nb").line(100), 2);
        // no trailing newline: last char is still on the last line
        assert_eq!(Source::new("x\ny").line(2), 2);
    }

    // T-007: over every byte offset across newline-edge cases, Source::line
    // must equal the original scan exactly — proves the perf refactor changed
    // no line numbers (#28).
    #[test]
    fn source_line_matches_naive_over_all_offsets() {
        let cases = [
            "",
            "\n",
            "no newlines here",
            "a\nb\nc\n",
            "\n\n\n",
            "line1\nline2\nline3",
            "trailing\n",
            "α\nβ\nγ", // multibyte: offsets are byte positions
        ];
        for source in cases {
            let index = Source::new(source);
            let len = u32::try_from(source.len()).unwrap();
            for offset in 0..=len + 2 {
                // Real spans land on char boundaries; the naive scan slices
                // `source[..offset]` and would panic mid-codepoint, so only the
                // boundary offsets are in its domain.
                if (offset as usize) <= source.len() && !source.is_char_boundary(offset as usize) {
                    continue;
                }
                assert_eq!(
                    index.line(offset),
                    naive_offset_to_line(source, offset),
                    "offset {offset} in {source:?}"
                );
            }
        }
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

    // T-001: node:test callback 内の `assert.equal` を assertion として数え
    // weak-assertion を発火しない
    #[test]
    fn node_test_callback_内の_assert_equal_を_assertion_として数え_weak_assertion_を発火しない() {
        let source = r#"test("name", async () => { assert.equal(result, 5); })"#;
        let blocks = parse(source);
        assert!(!blocks[0].assertions.is_empty());
        let issues = check_weak_assertions(&blocks, Path::new("a.ts"));
        assert!(
            !issues.iter().any(|i| i.rule == "weak-assertion"),
            "expected no weak-assertion, got: {:?}",
            issues.iter().map(|i| i.rule).collect::<Vec<_>>()
        );
    }

    // T-002: bare `assert(x)` を weak assertion として認識する
    #[test]
    fn bare_assert_x_を_weak_assertion_として認識する() {
        let blocks = parse(r#"test("name", () => { assert(cond); })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert!(blocks[0].assertions[0].is_weak);
    }

    // T-003: `assert.strictEqual` を strong assertion として分類する
    #[test]
    fn assert_strictequal_を_strong_assertion_として分類する() {
        let blocks = parse(r#"test("name", () => { assert.strictEqual(a, b); })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert!(!blocks[0].assertions[0].is_weak);
    }

    // T-004: `assert.ok` のみのテストを weak として weak-assertion 発火する
    #[test]
    fn assert_ok_のみのテストを_weak_として_weak_assertion_発火する() {
        let blocks = parse(r#"test("name", () => { assert.ok(v); })"#);
        let issues = check_weak_assertions(&blocks, Path::new("a.ts"));
        assert!(
            issues
                .iter()
                .any(|i| i.rule == "weak-assertion" && i.detail.contains("only weak")),
            "expected weak-assertion with 'only weak' detail, got: {:?}",
            issues
        );
    }

    // #98: `assert.fail(message)`'s argument is a failure message, not an
    // asserted value, so it must not be recorded as the assertion target and
    // must count as weak (no real value comparison happened).
    #[test]
    fn assert_fail_のメッセージ引数をtargetとして誤抽出しない() {
        let blocks = parse(r#"test("name", () => { assert.fail("should not reach here"); })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
        let assertion = &blocks[0].assertions[0];
        assert!(assertion.is_weak, "assert.fail must be classified as weak");
        assert_eq!(
            assertion.target, "",
            "assert.fail's message must not be recorded as the target"
        );
        assert_eq!(assertion.target_kind, TargetKind::Other);
    }

    // T-005: コールバック内 assert のみのテストが missing-act を発火しない
    //
    // The body's only call is the node:assert call itself (no other production
    // call), so this discriminates `is_act_call`/`is_assertion_call` treating
    // `assert.equal` as an Act (current, unfixed behavior: has_act wrongly
    // becomes true, masking a real missing-act) from treating it as an
    // assertion (target behavior: has_act stays false).
    #[test]
    fn コールバック内_assert_のみのテストが_missing_act_を発火しない() {
        let blocks = parse(r#"test("name", () => { assert.equal(result, 1); })"#);
        assert!(
            !blocks[0].has_act,
            "assert.equal alone must not be classified as an Act call"
        );
    }

    // T-005b: an arranged value asserted via node:assert with no real Act call
    // is missing-act. Under the current, unfixed classification, `assert.equal`
    // itself is wrongly treated as the Act, so missing-act stays silent (false
    // negative). Once U-002 makes `is_assertion_call` recognize node:assert
    // calls, `has_act` is false and `asserts_arranged` fires missing-act.
    #[test]
    fn assert_のみでarrangeした値を検証するテストはmissing_actを発火する() {
        let blocks = parse(r#"test("name", () => { const r = 5; assert.equal(r, 5); })"#);
        let issues = check_missing_act(&blocks, Path::new("a.ts"));
        assert!(
            issues.iter().any(|i| i.rule == "missing-act"),
            "expected missing-act, got: {:?}",
            issues.iter().map(|i| i.rule).collect::<Vec<_>>()
        );
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

    // T-313: an assertion inside a forEach callback is collected, not lost. The
    // forEach call is not assertion/mock, so the scan descends into its callback
    // argument body (#88 weak-assertion false positive on the legit loop form).
    #[test]
    fn collects_assertion_inside_foreach_callback() {
        let source = r#"test("table", () => {
            const cases = [{ a: 1, b: 2 }];
            cases.forEach(({ a, b }) => {
                expect(a + 1).toBe(b)
            })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].matcher, "toBe");
    }

    // T-314: an assertion inside an awaited waitFor callback is collected (#88,
    // the ubiquitous React Testing Library async form). `getByText` throws when
    // absent, so `toBeTruthy` is not weak here (#91).
    #[test]
    fn collects_assertion_inside_waitfor_callback() {
        let source = r#"test("async", async () => {
            await waitFor(() => {
                expect(screen.getByText("ok")).toBeTruthy()
            })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert!(!blocks[0].assertions[0].is_weak);
    }

    // T-317: `expect(getByText(...)).toBeTruthy()` is not weak — the throwing
    // query already guarantees the element exists, in both `screen.getByText`
    // member and bare `getByText` forms (#91).
    #[test]
    fn throwing_query_arg_is_not_weak() {
        let member = parse(
            r#"test("found", () => {
            expect(screen.getByText("ok")).toBeTruthy()
        })"#,
        );
        assert!(!member[0].assertions[0].is_weak);

        let bare = parse(
            r#"test("found", () => {
            expect(getAllByRole("button")).toBeTruthy()
        })"#,
        );
        assert!(!bare[0].assertions[0].is_weak);

        let awaited = parse(
            r#"test("found", async () => {
            expect(await screen.findByText("ok")).toBeTruthy()
        })"#,
        );
        assert!(!awaited[0].assertions[0].is_weak);
    }

    // T-318: `queryBy*` returns null instead of throwing, so a weak matcher on
    // its result stays weak (#91).
    #[test]
    fn query_by_arg_stays_weak() {
        let blocks = parse(
            r#"test("maybe", () => {
            expect(screen.queryByText("ok")).toBeTruthy()
        })"#,
        );
        assert!(blocks[0].assertions[0].is_weak);
    }

    // T-319: a non-query call result (callee is itself a call, not a query
    // identifier/member) keeps the weak matcher weak — the throwing-query
    // exclusion only applies to real queries (#91).
    #[test]
    fn non_query_call_arg_stays_weak() {
        let blocks = parse(
            r#"test("maybe", () => {
            expect(factory()()).toBeTruthy()
        })"#,
        );
        assert!(blocks[0].assertions[0].is_weak);
    }

    // T-320: an argument-less `expect()` still yields a (weak) assertion; the
    // throwing-query check sees no argument and leaves the matcher weak (#91).
    #[test]
    fn arg_less_expect_stays_weak() {
        let blocks = parse(
            r#"test("empty", () => {
            expect().toBeTruthy()
        })"#,
        );
        assert_eq!(blocks[0].assertions.len(), 1);
        assert!(blocks[0].assertions[0].is_weak);
    }

    // T-315: `it.each(table)(name, fn)` double-call shape is recognized as a test
    // and its inner callback assertion is captured (#88 false negative).
    #[test]
    fn recognizes_it_each_double_call() {
        let source = r#"it.each([[1, 2], [2, 3]])("adds %i", (a, b) => {
            expect(a).toBe(b)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "adds %i");
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-316: `describe.each(table)(name, fn)` routes into the nested test block.
    #[test]
    fn recognizes_describe_each_double_call() {
        let source = r#"describe.each([[1], [2]])("group %i", (n) => {
            it("works", () => {
                expect(n).toBe(n)
            })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "works");
    }

    // T-317: vitest's 3-arg `test(name, options, fn)` form exposes the callback,
    // so the block and its assertion are no longer invisible (#88).
    #[test]
    fn collects_callback_in_three_arg_test() {
        let source = r#"test("opts", { timeout: 1000 }, () => {
            expect(x).toBe(1)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-318 (guard): a helper assigned to a const but never invoked is a
    // definition, not an inline callback. Its expect must NOT count as an
    // assertion, or weak-assertion would be silenced (#88 false-negative guard).
    #[test]
    fn does_not_descend_into_uncalled_helper_definition() {
        let source = r#"test("defines but never asserts", () => {
            const helper = () => { expect(x).toBe(1) };
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 0);
    }

    // T-319 (symmetry): the outer iteration call is itself an Act, so a forEach
    // callback that both acts and asserts keeps has_act true — no missing-act
    // false positive once the callback assertion becomes visible (#88).
    #[test]
    fn foreach_callback_keeps_act_visible() {
        let source = r#"test("acc", () => {
            const acc = [];
            arr.forEach((x) => {
                acc.push(x)
                expect(acc).toContain(x)
            })
        })"#;
        let blocks = parse(source);
        assert!(!blocks[0].assertions.is_empty());
        assert!(blocks[0].has_act);
    }

    // T-320: descent reaches a try/catch nested in a callback, so a swallowed
    // error inside a forEach body is recorded — the cross-rule FN chain #88 names
    // (callback-internal catch-swallow / mock / dummy) is closed by routing the
    // callback through the full body walk, not an assertions-only probe.
    #[test]
    fn records_catch_swallow_inside_callback() {
        let source = r#"test("x", () => {
            items.forEach(() => {
                try { doThing() } catch {}
            })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }

    // T-321: `test.each(table)(name, fn)` is the third `.each` host alongside
    // `it`/`describe`; an unrecognized double-call makes the whole block invisible
    // (no rule reports), so the test set member `test` is pinned explicitly (#88).
    #[test]
    fn recognizes_test_each_double_call() {
        let source = r#"test.each([[1, 2]])("adds %i", (a, b) => {
            expect(a).toBe(b)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-322: the descent comment claims callback-internal mocks become real; a
    // `vi.fn()` defined inside a forEach body is recorded, pinning the mock half
    // of that contract (T-320 pinned only catch-swallow) (#88).
    #[test]
    fn records_mock_inside_callback() {
        let source = r#"test("x", () => {
            arr.forEach(() => {
                const m = vi.fn()
            })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].mock_calls.len(), 1);
    }

    // T-323: the descent comment also names dummies as a sink it closes; a dummy
    // literal inside a forEach body is recorded, pinning the dummy half of the
    // contract (#88).
    #[test]
    fn records_dummy_inside_callback() {
        let source = r#"test("x", () => {
            arr.forEach(() => {
                build("foo")
            })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].dummy_literals.len(), 1);
    }

    // T-324: descent recurses through nested callbacks (forEach inside forEach),
    // so an assertion two callback levels deep is still collected (#88).
    #[test]
    fn collects_assertion_in_nested_callback() {
        let source = r#"test("x", () => {
            outer.forEach(() => {
                inner.forEach(() => {
                    expect(a).toBe(b)
                })
            })
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-325 (negative guard): a double-call whose `.each` host is not a test
    // identifier (`db.each(rows)(...)`) is not a test, so it produces no block —
    // the new CallExpression arm's identifier filter must reject it (#88).
    #[test]
    fn non_test_each_double_call_is_not_a_block() {
        let source = r#"db.each(rows)((row) => {
            expect(row).toBe(1)
        })"#;
        let blocks = parse(source);
        assert!(blocks.is_empty());
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

    // TC-005: jest.fn/jest.mock/jest.spyOn are recognized as mocks, symmetric
    // with vi.* so mock-overuse fires on Jest suites too (#89 B3).
    #[test]
    fn recognizes_jest_mock_members() {
        let source = r#"test("x", () => {
            jest.fn()
            jest.mock("./module")
            jest.spyOn(obj, "method")
            expect(x).toBe(1)
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].mock_calls.len(), 3);
        assert_eq!(blocks[0].mock_calls[0].kind, MockKind::ViFn);
        assert_eq!(blocks[0].mock_calls[1].kind, MockKind::ViMock);
        assert_eq!(blocks[0].mock_calls[2].kind, MockKind::ViSpyOn);
    }

    // #89 B3: jest.* feeds act detection too (via is_act_call → mock_kind), not
    // only mock-overuse. A body whose only call is jest.fn() is a mock setup, so
    // it has no act — symmetric with vi.fn(). Guards against a future act-path
    // change that re-checks `vi` directly and silently flips Jest tests.
    #[test]
    fn jest_mock_only_body_has_no_act() {
        let blocks = parse(r#"test("x", () => { jest.fn() })"#);
        assert!(!blocks[0].has_act);
    }

    // Unrelated member expressions (neither vi nor jest) stay non-mocks (#89 B3).
    #[test]
    fn ignores_unrelated_member_fn() {
        let source = r#"test("x", () => {
            foo.fn()
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

    // T-128: logical-AND right operand assertion is detected (#26 symptom A)
    #[test]
    fn logical_and_right_operand_detected() {
        let blocks = parse(r#"test("x", () => { cond && expect(value).toBe(42) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-129: logical-AND right operand runs conditionally → IfBranch
    #[test]
    fn logical_and_right_operand_is_if_branch() {
        let blocks = parse(r#"test("x", () => { cond && expect(value).toBe(42) })"#);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::IfBranch);
    }

    // T-130: logical left operand runs unconditionally → keeps parent context
    #[test]
    fn logical_left_operand_unconditional() {
        let blocks = parse(r#"test("x", () => { expect(a).toBe(1) && expect(b).toBe(2) })"#);
        assert_eq!(blocks[0].assertions.len(), 2);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::TopLevel);
        assert_eq!(blocks[0].assertions[1].context, AssertionContext::IfBranch);
    }

    // T-131: ternary branch tautological is detected (#26 symptom B)
    #[test]
    fn ternary_branch_tautological_detected() {
        let blocks = parse(r#"test("x", () => { cond ? expect(true).toBe(true) : null })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].target_kind, TargetKind::Literal);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::IfBranch);
    }

    // T-132: both ternary branches are scanned
    #[test]
    fn ternary_both_branches_detected() {
        let blocks = parse(r#"test("x", () => { cond ? expect(a).toBe(1) : expect(b).toBe(2) })"#);
        assert_eq!(blocks[0].assertions.len(), 2);
    }

    // T-135: ternary test operand runs unconditionally → keeps parent context
    #[test]
    fn ternary_test_operand_unconditional() {
        let blocks = parse(r#"test("x", () => { expect(a).toBe(1) ? x : y })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::TopLevel);
    }

    // T-133: assertion wrapped in parentheses is detected
    #[test]
    fn parenthesized_assertion_detected() {
        let blocks = parse(r#"test("x", () => { (cond && expect(x).toBe(1)) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::IfBranch);
    }

    // T-134: a function-expression callback argument IS traversed (#88 lifts the
    // #26 boundary). The author wrote the assertion to run inside the callback, so
    // it counts even when the call (setTimeout/forEach/waitFor) defers it — the
    // common forEach/waitFor false positive matters more than the rare
    // setTimeout-only false negative, and the AST cannot tell them apart.
    #[test]
    fn call_argument_callback_traversed() {
        let blocks = parse(r#"test("x", () => { setTimeout(() => expect(x).toBe(1)) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-136: assertion in a sequence expression is detected, unconditionally (#31)
    #[test]
    fn sequence_expression_assertion_detected() {
        let blocks = parse(r#"test("x", () => { (doThing(), expect(v).toBe(1)) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
        assert_eq!(blocks[0].assertions[0].context, AssertionContext::TopLevel);
    }

    // T-137: expect base wrapped in `as` cast is detected (#31)
    #[test]
    fn ts_as_wrapped_expect_detected() {
        let blocks = parse(r#"test("x", () => { (expect(v) as any).toBe(1) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-138: expect base with non-null assertion is detected (#31)
    #[test]
    fn ts_non_null_wrapped_expect_detected() {
        let blocks = parse(r#"test("x", () => { expect(v)!.toBe(1) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-139: parenthesized expect base is detected (#31)
    #[test]
    fn parenthesized_expect_base_detected() {
        let blocks = parse(r#"test("x", () => { (expect(v)).toBe(1) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-140: expect base wrapped in `satisfies` is detected (#31)
    #[test]
    fn ts_satisfies_wrapped_expect_detected() {
        let blocks = parse(r#"test("x", () => { (expect(v) satisfies unknown).toBe(1) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
    }

    // T-142: a `satisfies`/type-asserted expect argument still resolves its root,
    // so missing-act sees the target (expr_root_ident mirrors find_expect_target, #31)
    #[test]
    fn ts_wrapped_expect_argument_root_resolved() {
        let blocks = parse(r#"test("x", () => { expect(v satisfies unknown).toBe(1) })"#);
        assert_eq!(blocks[0].assertions[0].target_root.as_deref(), Some("v"));
    }

    // T-141: a whole assertion call wrapped in a cast is detected (#31)
    #[test]
    fn ts_as_wrapped_assertion_call_detected() {
        let blocks = parse(r#"test("x", () => { (expect(v).toBe(1) as any) })"#);
        assert_eq!(blocks[0].assertions.len(), 1);
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

    // T-112b: rethrow nested in an if → no catch_swallow (#27 false positive)
    #[test]
    fn nested_rethrow_in_catch_no_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                if (e) { throw e }
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_swallows.is_empty());
    }

    // T-112c: rethrow nested in a block/for/try inside the catch → no swallow
    #[test]
    fn deeply_nested_rethrow_in_catch_no_swallow() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                for (const k of keys) { { throw e } }
            }
        })"#;
        let blocks = parse(source);
        assert!(blocks[0].catch_swallows.is_empty());
    }

    // T-112d: a throw only inside a nested callback is not a synchronous
    // rethrow, so the catch still swallows.
    #[test]
    fn throw_in_catch_callback_still_swallows() {
        let source = r#"test("x", () => {
            try {
                riskyOp()
            } catch (e) {
                items.forEach(() => { throw e })
            }
        })"#;
        let blocks = parse(source);
        assert_eq!(blocks[0].catch_swallows.len(), 1);
    }

    // T-112e: rethrow nested in each control-flow construct → no swallow (#27)
    #[test]
    fn nested_rethrow_in_each_construct_no_swallow() {
        let catch_bodies = [
            "for (let i = 0; i < 1; i++) { throw e }",
            "for (const k in obj) { throw e }",
            "while (e) { throw e }",
            "do { throw e } while (e)",
            "switch (e) { default: throw e }",
            "try { throw e } finally {}",
            "try { x() } catch (inner) { throw inner }",
            "try { x() } finally { throw e }",
        ];
        for body in catch_bodies {
            let source =
                format!(r#"test("x", () => {{ try {{ risky() }} catch (e) {{ {body} }} }})"#);
            let blocks = parse(&source);
            assert!(
                blocks[0].catch_swallows.is_empty(),
                "should not swallow with catch body: {body}"
            );
        }
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

    // T-416: the transparent-wrapper set must stay synchronized across the four
    // traversal functions that see through wrappers — scan_expr (assertion
    // discovery), find_expect_target (matcher target), expr_root_ident (target
    // root), expr_has_act (act detection). This is a checklist, not a mechanical
    // drift detector: it verifies each LISTED wrapper is transparent on all four
    // paths. Adding a new transparent wrapper still requires human discipline to
    // update all four functions AND this list; only a single shared unwrap
    // helper would make divergence structurally impossible (issue #70). Listing
    // the set in one place keeps the four-way agreement reviewable in one test.
    #[test]
    fn transparent_wrappers_stay_synced_across_traversals() {
        // (label, prefix, suffix) per wrapper. Parsed as `.ts`: the
        // type-assertion form `<any>x` is JSX-ambiguous and invalid in `.tsx`.
        let wrappers: &[(&str, &str, &str)] = &[
            ("paren", "(", ")"),
            ("as", "", " as any"),
            ("satisfies", "", " satisfies unknown"),
            ("non-null", "", "!"),
            ("type-assertion", "<any>", ""),
        ];

        for (label, pre, suf) in wrappers {
            // scan_expr: a wrapped assertion statement is still discovered.
            let blocks = parse_ts(&format!(
                r#"test("t", () => {{ {pre}expect(v).toBe(1){suf} }})"#
            ));
            assert_eq!(
                blocks[0].assertions.len(),
                1,
                "scan_expr lost the assertion through `{label}`"
            );

            // find_expect_target: a wrapper between expect() and its matcher
            // still resolves the target. Outer parens keep suffix wrappers a
            // valid member receiver.
            let blocks = parse_ts(&format!(
                r#"test("t", () => {{ ({pre}expect(user.name){suf}).toBe(1) }})"#
            ));
            assert_eq!(
                blocks[0].assertions[0].target, "user.name",
                "find_expect_target lost the target through `{label}`"
            );

            // expr_root_ident: a wrapper around the expect argument still
            // resolves the root identifier.
            let blocks = parse_ts(&format!(
                r#"test("t", () => {{ expect({pre}user{suf}.name).toBe(1) }})"#
            ));
            assert_eq!(
                blocks[0].assertions[0].target_root.as_deref(),
                Some("user"),
                "expr_root_ident lost the root through `{label}`"
            );

            // expr_has_act: a wrapped production call is still an Act.
            let blocks = parse_ts(&format!(r#"test("t", () => {{ {pre}doWork(){suf} }})"#));
            assert!(
                blocks[0].has_act,
                "expr_has_act lost the act through `{label}`"
            );
        }
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

    // T-310: a production call in a destructuring default value is an Act. Without
    // scanning the binding pattern, `const { a = run() } = obj` hides its only call
    // in the default and missing-act fires a false positive.
    #[test]
    fn has_act_true_for_destructuring_default_call() {
        let blocks = parse(r#"test("x", () => { const { a = run() } = obj; expect(a).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-311: an array-destructuring default reaches the same Act through a distinct
    // pattern arm (`ArrayPattern` rather than `ObjectPattern`), so it needs its own
    // contract; a refactor breaking the array recursion would leave T-310 green.
    #[test]
    fn has_act_true_for_array_destructuring_default_call() {
        let blocks = parse(r#"test("x", () => { const [a = run()] = arr; expect(a).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }

    // T-312: a computed destructuring key is the last spot a call can hide in a
    // binding pattern (`const { [run()]: a } = obj`). The key, not the value, carries
    // the Act, so scanning only `prop.value` would let missing-act fire a false positive.
    #[test]
    fn has_act_true_for_computed_destructuring_key_call() {
        let blocks = parse(r#"test("x", () => { const { [run()]: a } = obj; expect(a).toBe(1) })"#);
        assert!(blocks[0].has_act);
    }
}
