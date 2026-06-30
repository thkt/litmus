**English** | [日本語](README.ja.md)

### litmus

Test quality linter for TypeScript/JavaScript. Detects tests that pass but don't actually verify behavior.

#### Problem

Tests can be green and meaningless. CI passes, coverage looks fine, but the tests don't catch bugs:

```typescript
// Passes. Tests nothing.
test("works", () => {
  expect(true).toBe(true);
});

// Passes. Only checks mocks were called, not what the function returned.
test("fetch", () => {
  expect(mockApi).toHaveBeenCalledWith("/users");
  expect(mockApi).toHaveBeenCalledTimes(1);
});

// Passes. 3 mocks, 1 assertion. More scaffolding than verification.
test("submit", () => {
  const a = vi.fn();
  const b = vi.fn();
  const c = vi.fn();
  expect(result).toBe(1);
});
```

LLM-generated tests amplify this problem — they produce syntactically valid tests that satisfy coverage tools but assert on the wrong things.

#### What litmus detects

| Rule                    | Severity | Detects                                               | Example                                               |
| ----------------------- | -------- | ----------------------------------------------------- | ----------------------------------------------------- |
| `weak-assertion`        | block    | Only weak matchers or no assertions                   | `expect(x).toBeTruthy()` as sole assertion            |
| `tautological`          | block    | Assertions on literal values that always pass         | `expect(true).toBe(true)`                             |
| `mock-overuse`          | block    | Mock setup exceeds assertions                         | 3 `vi.fn()` calls, 1 `expect`                         |
| `mock-only`             | block    | Verifies only mock interactions                       | Only `toHaveBeenCalledWith` / `toHaveBeenCalledTimes` |
| `missing-act`           | warn     | Asserts on arranged data with no SUT call             | `const x = 42; expect(x).toBe(42)`                    |
| `dummy-data`            | warn     | Placeholder literals (foo/bar/baz/qux/hoge/fuga)      | `createUser({ name: "foo" })`                         |
| `snapshot-external`     | warn     | Asserts against an external snapshot file             | `expect(html).toMatchSnapshot()`                      |
| `empty-test`            | block    | Test body is empty (`.todo` excluded)                 | `test("works", () => {})`                             |
| `skipped-test`          | block    | Test is skipped or marked todo                        | `test.skip("works", ...)`                             |
| `catch-swallow`         | block    | Catch block has no assertion and no throw             | `try { act() } catch {}`                              |
| `catch-masks-assertion` | block    | Try asserts and catch asserts, swallowing the failure | `try { expect(a) } catch { expect(b) }`               |
| `catch-only-assertion`  | block    | Every assertion lives inside catch                    | assertions only in the `catch` block                  |
| `conditional-assertion` | block    | Every assertion lives inside `if`                     | `if (x) { expect(...) }`                              |

Based on [javascript-testing-best-practices](https://github.com/goldbergyoni/javascript-testing-best-practices) by Yoni Goldberg.

#### How it works

litmus parses test files with [oxc](https://oxc.rs) (the same parser behind oxlint), walks the AST to extract test blocks, and applies rule checks against the structure — not regex, not string matching.

```
$ litmus ./src
weak-assertion: src/auth.test.ts:15 handles login (only weak: toBeTruthy)
mock-only: src/api.test.ts:42 fetches users (matchers: toHaveBeenCalledWith, toHaveBeenCalledTimes)
```

#### Exit codes

| Code | Meaning                                                              |
| ---- | -------------------------------------------------------------------- |
| 0    | clean (no violations)                                                |
| 1    | warn-level violations only (advisory)                                |
| 2    | blocking violations found                                            |
| 64   | usage error (invalid CLI arguments)                                  |
| 70   | internal error (panic / invariant violation / per-file worker crash) |

Codes 64 and 70 follow [sysexits.h](https://man.openbsd.org/sysexits.3) conventions; codes 0/1/2 follow the hook-tool convention (pass / warn / block). Warn-level rules (`missing-act`, `dummy-data`, `snapshot-external`) emit 1; all other rules emit 2. When both are present, 2 takes precedence.

#### Installation

##### From source

```bash
git clone https://github.com/thkt/litmus.git
cd litmus
cargo build --release
cp target/release/litmus ~/.local/bin/
```

#### Usage

```bash
# Scan current directory
litmus .

# Scan specific directory
litmus ./src

# In CI (non-zero exit blocks the pipeline)
litmus . || exit 1

# Machine-readable output for agents and tooling
litmus --json ./src
```

With `--json`, stdout carries a single `{"issues":[...],"errors":[...]}` document and CLI errors print an error object with `next_step` and `candidates` to stderr. Exit codes are unchanged. Piping to a reader that closes early (`litmus | head`) stops cleanly at exit 0 instead of crashing.

#### Supported file patterns

litmus scans the `**/*.test.*` and `**/*.spec.*` globs, then keeps files whose extension is one of:

- `.ts` `.tsx` `.js` `.jsx` `.mjs` `.cjs` `.mts` `.cts`

Automatically excludes: `node_modules/`, `.git/`, `dist/`, `build/`, `target/`

#### Design decisions

- **AST over regex**: Pattern matching on source text produces false positives on comments, strings, and nested expressions. AST analysis is precise.
- **Conservative thresholds**: Every rule is tuned to minimize false positives. Real-world validated against 4 codebases with 0 FP.
- **No config needed**: Sensible defaults. No `.litmusrc`, no plugins, no setup.
- **Fast**: Single Rust binary, oxc parser. Scans hundreds of test files in milliseconds.

#### Roadmap

See [Issues](https://github.com/thkt/litmus/issues) for planned rules:

- [#3](https://github.com/thkt/litmus/issues/3) Shared test state detection
