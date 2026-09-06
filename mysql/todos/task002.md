# task002: `REVERSE` と `REPEAT` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Not looked at → Strings」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

- `REVERSE(str)` — 文字列を逆順にする。1 引数。
- `REPEAT(str, n)` — 文字列を n 回繰り返す。2 引数。

**エンジン側の名前に注意。** `core/function.rs` によれば、繰り返しは `repeat` だが、
逆順は **`string_reverse`** であって `reverse` ではない。実装前に `core/function.rs` で確認すること。

2 つを 1 タスクにまとめてあるのは、どちらも同じ 4 箇所を触る同型の作業だから。
片方だけ先に入れてもよい。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), note TEXT);
INSERT INTO t VALUES (1, 'abc', 'hello');

SELECT REVERSE(name) FROM t;
SELECT REVERSE('あいう');          -- マルチバイトが文字単位で反転するか
SELECT REVERSE(NULL);
SELECT REPEAT(name, 3) FROM t;
SELECT REPEAT(name, 0) FROM t;     -- 0 回
SELECT REPEAT(name, -1) FROM t;    -- 負数
SELECT REPEAT(NULL, 3);
SELECT REPEAT(name, NULL) FROM t;
```

`--column-type-info` で結果列を見て、次を記録する。

- `REVERSE` の `column_length` は元の列幅と同じか
- `REPEAT(col, n)` の `column_length` は **`元の幅 × n`** になるか。ここが核心。
  n がリテラルでないと幅が決まらないので、実装では n をリテラルに限ることになる。
- どちらも `NOT_NULL` が立つか

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT string_reverse('abc'), string_reverse('あいう'), repeat('ab', 3), repeat('ab', 0);
SQL
```

マルチバイトの扱いが MySQL とエンジンで食い違うなら、その事実を COMPAT.md に書く。

## 実装

README.md の「スカラ関数を 1 つ足すときの型」に従う。

- `REVERSE` は `scalar_call()` の末尾にある「1 引数・引数は列」の分岐群に足すのが素直。
  幅が元のままなら `ScalarFunction::KeepsTextShape` をそのまま使える。
  ただし `KeepsTextShape` は `scalar_call_column_definition()` で
  `column_type` を `MYSQL_TYPE_VAR_STRING` に固定している。`REVERSE` の計測結果が
  それと違うなら、`KeepsTextShape` を流用せず新しい variant を足すこと。
- `REPEAT` は `LEFT` / `RIGHT` の分岐が最も近い。第 2 引数を数値リテラルに限り、
  `literal_characters` に回数を入れ、`scalar_call_column_definition()` で
  `元の列の character_length × 回数 × UTF8MB4_MAX_BYTES_PER_CHARACTER` を組み立てる。
  `saturating_mul` を使い、桁あふれで包まないこと。
- `render_scalar_call()` では `string_reverse(...)` / `repeat(...)` に落とす。

## テスト

- パーサ: 翻訳後 SQL が `string_reverse("name")` / `repeat("name", 3)` になること。
  `REPEAT(name, ?)` や `REPEAT(name, id)` のように回数がリテラルでないものが拒否されること。
- エンドツーエンド: 値と結果列メタデータ。特に `REPEAT` の `column_length` が
  「元の幅 × 回数」になっていること。

## ドキュメント

- `mysql/TODO.md` の「Strings」行から `REVERSE`、`REPEAT` を消す。
- マルチバイトや負の回数で食い違うなら `mysql/COMPAT.md` に書く。
