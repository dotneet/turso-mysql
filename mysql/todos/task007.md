# task007: `NULLIF` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Conditional expressions」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`NULLIF(a, b)` — `a = b` なら NULL、そうでなければ `a`。
エンジンにも `nullif` がある（`core/function.rs`）。名前も引数の並びも一致している。

`IFNULL` / `COALESCE`（`ScalarFunction::Defaulted`）がすでに入っているので、
その分岐が直接の手本になる。ただし **`NULLIF` は逆で、結果は必ず NULL になりうる**。
`Defaulted` は `not_null: true` を返しているので、そのまま流用してはいけない。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT NOT NULL, s VARCHAR(20) NOT NULL);
INSERT INTO t VALUES (1, 5, 'abc');

SELECT NULLIF(n, 5) FROM t;
SELECT NULLIF(n, 9) FROM t;
SELECT NULLIF(s, 'abc') FROM t;
SELECT NULLIF(s, 'ABC') FROM t;   -- 大文字小文字（照合順序が効くか）
SELECT NULLIF(NULL, 1);
SELECT NULLIF(1, NULL);
```

`--column-type-info` で記録する。

- 結果の型と長さが**第 1 引数のものと同じか**
- **`NOT_NULL` フラグが落ちるか。** 第 1 引数が `NOT NULL` 列でも、`NULLIF` の結果は
  NULL になりうるので落ちるはず。実測で確認する。
- `NULLIF(s, 'ABC')` が NULL を返すか（照合順序が比較に効くか）

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT nullif(5,5), nullif(5,9), nullif('abc','abc'), nullif('abc','ABC');
SQL
```

**大文字小文字の扱いが食い違う可能性が高い。** MySQL の既定照合順序は case-insensitive なので
`NULLIF('abc','ABC')` が NULL になるかもしれないが、エンジンは区別するはず。
食い違うなら、テキスト引数のときに `COLLATE NOCASE` を挟むか（既存の
`render_checked_select_comparison` が同じ問題を同じ方法で解いている）、
テキスト引数を拒否する。

## 実装

- `mysql/parser/static_select_metadata.rs` の `scalar_call()`:
  `IFNULL` / `COALESCE` の分岐のすぐ下に `named(&["NULLIF"])` を足す。
  - 第 1 引数は `Expr::Identifier`（列）に限る。第 2 引数はリテラル（整数または文字列）に限る。
  - 新しい `ScalarFunction` variant（例: `NullsOnMatch`）を足すか、
    `Defaulted` に `not_null` を渡す形へ寄せる。**`not_null: false` にすること。**
- `mysql/parser/translate.rs` の `render_scalar_call()`:
  `nullif(<arg0>, <arg1>)`。テキストで照合順序を合わせる必要があるなら
  `nullif(<arg0> COLLATE NOCASE, <arg1>)` の形になるかを `tursodb` で確かめてから決める。
- `mysql/server/src/frontend_adapter.rs` の `scalar_call_column_definition()`:
  第 1 引数の列の形をそのまま返しつつ、`MYSQL_NOT_NULL_FLAG` を**落とす**。
  `own_shape(name)` を使った後にフラグを調整する。

## テスト

- パーサ: 翻訳後 SQL。第 1 引数が列でないもの、第 2 引数がリテラルでないものの拒否。
- エンドツーエンド: **`NOT NULL` 列に対する `NULLIF` の結果列から
  `NOT_NULL` フラグが落ちていること**を明示的に確認する。これがこのタスク固有の要点。
  一致する場合・しない場合の値も見る。

## ドキュメント

- `mysql/TODO.md` の「Conditional expressions」表から `NULLIF` の行を消す。
- 照合順序で食い違うなら `mysql/COMPAT.md` に書く。
