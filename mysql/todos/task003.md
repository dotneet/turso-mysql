# task003: `LPAD` / `RPAD` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Not looked at → Strings」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`LPAD(str, len, pad)` / `RPAD(str, len, pad)` — `str` を長さ `len` になるまで
`pad` で埋める。`str` が `len` より長ければ**切り詰める**。

エンジンには `lpad` / `rpad` がある（`core/function.rs`）。名前は一致している。
ただし引数の意味（文字数かバイト数か、切り詰めるか）が同じとは限らないので必ず計測する。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20));
INSERT INTO t VALUES (1, 'abc');

SELECT LPAD(name, 6, '*') FROM t;
SELECT RPAD(name, 6, '*') FROM t;
SELECT LPAD(name, 2, '*') FROM t;      -- len が元より短い（切り詰め）
SELECT LPAD(name, 0, '*') FROM t;
SELECT LPAD(name, 6, '') FROM t;       -- pad が空
SELECT LPAD(name, 6, 'xy') FROM t;     -- pad が複数文字
SELECT LPAD('あい', 4, '*');            -- マルチバイト（文字単位かバイト単位か）
SELECT LPAD(NULL, 6, '*');
```

`--column-type-info` で結果列を見て記録する。

- `column_length` は **`len` から決まる**はず（`len × UTF8MB4_MAX_BYTES_PER_CHARACTER`）。実測で確認。
- `column_type`、`NOT_NULL` フラグ

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT lpad('abc', 6, '*'), rpad('abc', 6, '*'), lpad('abc', 2, '*'), lpad('あい', 4, '*');
SQL
```

**切り詰めの挙動とマルチバイトの数え方が食い違いやすい。** 食い違ったら
COMPAT.md に書くか、その入力を拒否する。

## 実装

`LEFT` / `RIGHT` の分岐がそのまま雛形になる。両方とも
「列 1 つ + 幅を決める数値リテラル」という同じ構造なので、`ScalarFunction::TakesCharacters`
をそのまま使える可能性が高い（`literal_characters` に `len` を入れる）。

- `mysql/parser/static_select_metadata.rs` の `scalar_call()`:
  `named(&["LPAD", "RPAD"])` の分岐。引数は 3 つ。
  第 1 引数は `Expr::Identifier`、第 2 引数は数値リテラル、第 3 引数は文字列リテラルに限る。
- `mysql/parser/translate.rs` の `render_scalar_call()`:
  `lpad(<arg0>, <arg1>, <arg2>)` に落とす。
- `mysql/server/src/frontend_adapter.rs`:
  `TakesCharacters` を流用するなら追加の分岐は要らない。計測結果が違うなら新 variant を足す。

## テスト

- パーサ: 翻訳後 SQL、および `len` や `pad` がリテラルでないものの拒否。
- エンドツーエンド: 値（埋める場合・切り詰める場合の両方）と結果列の `column_length`。

## ドキュメント

- `mysql/TODO.md` の「Strings」行から `LPAD`、`RPAD` を消す。
- 切り詰めやマルチバイトで食い違うなら `mysql/COMPAT.md` に書く。
