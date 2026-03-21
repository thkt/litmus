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

| ルール              | 検出内容                             | 例                                                    |
| ------------------- | ------------------------------------ | ----------------------------------------------------- |
| `weak-assertion`    | 弱いmatcherのみ、またはassertionなし | `expect(x).toBeTruthy()` が唯一のassertion            |
| `mock-overuse`      | mock数がassertion数を超過            | `vi.fn()` が3つ、`expect` が1つ                       |
| `tautological`      | リテラル値への常に通るassertion      | `expect(true).toBe(true)`                             |
| `mock-only`         | mockの呼ばれ方だけを検証             | `toHaveBeenCalledWith` / `toHaveBeenCalledTimes` のみ |
| `test-name-quality` | 失敗時に原因が分からないテスト名     | `"works"`, `"should work"`                            |

Yoni Goldberg著 [javascript-testing-best-practices](https://github.com/goldbergyoni/javascript-testing-best-practices) に基づく。

#### 仕組み

litmusは [oxc](https://oxc.rs)（oxlintと同じパーサー）でテストファイルを解析し、ASTを走査してテストブロックを抽出、構造に対してルールチェックを適用する。正規表現でも文字列マッチングでもない。

```
$ litmus ./src
weak-assertion: src/auth.test.ts:15 handles login (only weak: toBeTruthy)
mock-only: src/api.test.ts:42 fetches users (matchers: toHaveBeenCalledWith, toHaveBeenCalledTimes)
test-name-quality: src/utils.test.ts:8 works (words: 1)
```

終了コード0 = クリーン。終了コード1 = issueあり。

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
```

#### 対応ファイルパターン

- `**/*.test.ts`
- `**/*.test.tsx`

自動除外: `node_modules/`, `.git/`, `dist/`, `build/`, `target/`

#### 設計判断

- **正規表現ではなくAST**: ソーステキストのパターンマッチングはコメント、文字列、ネスト式で誤検出する。AST解析は正確。
- **保守的な閾値**: 全ルールで誤検出を最小化。4つの実プロジェクトでFP 0件を確認済み。
- **設定不要**: デフォルトで動く。`.litmusrc` もプラグインもセットアップもなし。
- **高速**: Rust単一バイナリ + oxcパーサー。数百のテストファイルをミリ秒で走査。

#### ロードマップ

計画中のルールは [Issues](https://github.com/thkt/litmus/issues) を参照:

- [#1](https://github.com/thkt/litmus/issues/1) ダミーデータ検出 (`"foo"`, `"bar"`, `123`)
- [#2](https://github.com/thkt/litmus/issues/2) AAAパターンのAct不在検出
- [#3](https://github.com/thkt/litmus/issues/3) テスト間の共有状態検出
