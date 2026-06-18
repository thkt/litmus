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

| Rule                | Detects                                           | Example                                               |
| ------------------- | ------------------------------------------------- | ----------------------------------------------------- |
| `weak-assertion`    | Tests with only weak matchers or no assertions    | `expect(x).toBeTruthy()` as sole assertion            |
| `mock-overuse`      | Tests where mock setup exceeds assertions         | 3 `vi.fn()` calls, 1 `expect`                         |
| `tautological`      | Assertions on literal values that always pass     | `expect(true).toBe(true)`                             |
| `mock-only`         | Tests verifying only mock interactions            | Only `toHaveBeenCalledWith` / `toHaveBeenCalledTimes` |
| `test-name-quality` | Test names too vague to diagnose failures         | `"works"`, `"should work"`                            |
| `missing-act`       | Tests asserting on arranged data with no SUT call | `const x = 42; expect(x).toBe(42)`                    |

Based on [javascript-testing-best-practices](https://github.com/goldbergyoni/javascript-testing-best-practices) by Yoni Goldberg.

#### How it works

litmus parses test files with [oxc](https://oxc.rs) (the same parser behind oxlint), walks the AST to extract test blocks, and applies rule checks against the structure — not regex, not string matching.

```
$ litmus ./src
weak-assertion: src/auth.test.ts:15 handles login (only weak: toBeTruthy)
mock-only: src/api.test.ts:42 fetches users (matchers: toHaveBeenCalledWith, toHaveBeenCalledTimes)
test-name-quality: src/utils.test.ts:8 works (words: 1)
```

#### Exit codes

| Code | Meaning                                      |
| ---- | -------------------------------------------- |
| 0    | clean (no violations)                        |
| 1    | warn-level violations only (advisory)        |
| 2    | blocking violations found                    |
| 64   | usage error (invalid CLI arguments)          |
| 70   | internal error (panic / invariant violation) |

Codes 64 and 70 follow [sysexits.h](https://man.openbsd.org/sysexits.3) conventions; codes 0/1/2 follow the hook-tool convention (pass / warn / block). Warn-level rules (`missing-act`, `dummy-data`) emit 1; all other rules emit 2. When both are present, 2 takes precedence.

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
```

#### Supported file patterns

- `**/*.test.ts`
- `**/*.test.tsx`

Automatically excludes: `node_modules/`, `.git/`, `dist/`, `build/`, `target/`

#### Design decisions

- **AST over regex**: Pattern matching on source text produces false positives on comments, strings, and nested expressions. AST analysis is precise.
- **Conservative thresholds**: Every rule is tuned to minimize false positives. Real-world validated against 4 codebases with 0 FP.
- **No config needed**: Sensible defaults. No `.litmusrc`, no plugins, no setup.
- **Fast**: Single Rust binary, oxc parser. Scans hundreds of test files in milliseconds.

#### Roadmap

See [Issues](https://github.com/thkt/litmus/issues) for planned rules:

- [#1](https://github.com/thkt/litmus/issues/1) Dummy data detection (`"foo"`, `"bar"`, `123`)
- [#2](https://github.com/thkt/litmus/issues/2) Missing Act in AAA pattern
- [#3](https://github.com/thkt/litmus/issues/3) Shared test state detection
