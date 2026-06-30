[English](README.md) | **日本語**

### litmus

TypeScript/JavaScript向けテスト品質リンター。通るけど何も検証していないテストを検出する。

#### 課題

テストがグリーンでも意味がないことがある。CIは通る、カバレッジも出る、でもバグは素通り:

```typescript
// 通る。何もテストしていない。
test("works", () => {
  expect(true).toBe(true);
});

// 通る。関数の戻り値ではなく、mockが呼ばれたかだけを確認している。
test("fetch", () => {
  expect(mockApi).toHaveBeenCalledWith("/users");
  expect(mockApi).toHaveBeenCalledTimes(1);
});

// 通る。mockが3つ、assertionが1つ。検証よりセットアップの方が多い。
test("submit", () => {
  const a = vi.fn();
  const b = vi.fn();
  const c = vi.fn();
  expect(result).toBe(1);
});
```

LLM生成テストはこの問題を増幅する — 構文的には正しいがassertの対象が間違っているテストを量産する。

#### litmus が検出するもの

| ルール                  | 深刻度 | 検出内容                                       | 例                                                    |
| ----------------------- | ------ | ---------------------------------------------- | ----------------------------------------------------- |
| `weak-assertion`        | block  | 弱いmatcherのみ、またはassertionなし           | `expect(x).toBeTruthy()` が唯一のassertion            |
| `tautological`          | block  | リテラル値への常に通るassertion                | `expect(true).toBe(true)`                             |
| `mock-overuse`          | block  | mock数がassertion数を超過                      | `vi.fn()` が3つ、`expect` が1つ                       |
| `mock-only`             | block  | mockの呼ばれ方だけを検証                       | `toHaveBeenCalledWith` / `toHaveBeenCalledTimes` のみ |
| `missing-act`           | warn   | arrange済みデータにassertするがSUT呼び出しなし | `const x = 42; expect(x).toBe(42)`                    |
| `dummy-data`            | warn   | プレースホルダ値 (foo/bar/baz/qux/hoge/fuga)   | `createUser({ name: "foo" })`                         |
| `snapshot-external`     | warn   | 外部snapshotファイルに対するassertion          | `expect(html).toMatchSnapshot()`                      |
| `empty-test`            | block  | テスト本体が空 (`.todo` は除外)                | `test("works", () => {})`                             |
| `skipped-test`          | block  | skip または todo されたテスト                  | `test.skip("works", ...)`                             |
| `catch-swallow`         | block  | catch ブロックに assertion も throw もない     | `try { act() } catch {}`                              |
| `catch-masks-assertion` | block  | try と catch 双方が assert し失敗を握りつぶす  | `try { expect(a) } catch { expect(b) }`               |
| `catch-only-assertion`  | block  | 全 assertion が catch 内にある                 | `catch` ブロック内のみの assertion                    |
| `conditional-assertion` | block  | 全 assertion が `if` 内にある                  | `if (x) { expect(...) }`                              |

Yoni Goldberg著 [javascript-testing-best-practices](https://github.com/goldbergyoni/javascript-testing-best-practices) に基づく。

#### 仕組み

litmusは [oxc](https://oxc.rs)（oxlintと同じパーサー）でテストファイルを解析し、ASTを走査してテストブロックを抽出、構造に対してルールチェックを適用する。正規表現でも文字列マッチングでもない。

```
$ litmus ./src
weak-assertion: src/auth.test.ts:15 handles login (only weak: toBeTruthy)
mock-only: src/api.test.ts:42 fetches users (matchers: toHaveBeenCalledWith, toHaveBeenCalledTimes)
```

#### 終了コード

| Code | 意味                                                                   |
| ---- | ---------------------------------------------------------------------- |
| 0    | クリーン (違反なし)                                                    |
| 1    | warn レベルの違反のみ (advisory)                                       |
| 2    | blocking の違反を検出                                                  |
| 64   | usage エラー (CLI 引数が不正)                                          |
| 70   | 内部エラー (panic / invariant violation / ファイル単位の worker crash) |

64 と 70 は [sysexits.h](https://man.openbsd.org/sysexits.3) 準拠、0/1/2 は hook ツールの慣例 (pass / warn / block)。warn レベルのルール (`missing-act`, `dummy-data`, `snapshot-external`) は 1、それ以外のルールは 2 を返す。両方ある場合は 2 が優先。

#### インストール

##### ソースから

```bash
git clone https://github.com/thkt/litmus.git
cd litmus
cargo build --release
cp target/release/litmus ~/.local/bin/
```

#### 使い方

```bash
# カレントディレクトリをスキャン
litmus .

# 特定ディレクトリをスキャン
litmus ./src

# CI で使う（非ゼロ終了でパイプラインをブロック）
litmus . || exit 1

# エージェントやツール向けの機械可読出力
litmus --json ./src
```

`--json` 指定時は stdout に `{"issues":[...],"errors":[...]}` の単一ドキュメントを出力し、CLI エラーは `next_step` と `candidates` を持つエラーオブジェクトを stderr に出す。終了コードは変わらない。早期に close する読み手へのパイプ（`litmus | head`）はクラッシュせず exit 0 で正常終了する。

#### 対応ファイルパターン

`**/*.test.*` と `**/*.spec.*` の glob を走査し、拡張子が次のいずれかのファイルを対象とする:

- `.ts` `.tsx` `.js` `.jsx` `.mjs` `.cjs` `.mts` `.cts`

自動除外: `node_modules/`, `.git/`, `dist/`, `build/`, `target/`

#### 設計判断

- **正規表現ではなくAST**: ソーステキストのパターンマッチングはコメント、文字列、ネスト式で誤検出する。AST解析は正確。
- **保守的な閾値**: 全ルールで誤検出を最小化。4つの実プロジェクトでFP 0件を確認済み。
- **設定不要**: デフォルトで動く。`.litmusrc` もプラグインもセットアップもなし。
- **高速**: Rust単一バイナリ + oxcパーサー。数百のテストファイルをミリ秒で走査。

#### ロードマップ

計画中のルールは [Issues](https://github.com/thkt/litmus/issues) を参照:

- [#3](https://github.com/thkt/litmus/issues/3) テスト間の共有状態検出
