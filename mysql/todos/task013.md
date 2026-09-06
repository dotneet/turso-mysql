# task013: `UPDATE` / `DELETE` の `WHERE ... IN (...)` を通す

**難易度: 低** / 参照: `../TODO.md` 「SQL syntax → SELECT」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`DELETE FROM t WHERE id IN (1, 2)` と `UPDATE t SET x = 1 WHERE id IN (1, 2)`。
`SELECT` 側の `IN` リストはすでに通っている（`render_checked_in_list()`）。
DML 側だけが別の述語レンダラを使っているので落ちる。

## なぜ簡単か

`mysql/parser/translate.rs` に `render_select_predicate()` と `render_dml_predicate()` の
2 つがあり、`render_dml_predicate()` の `match` には `Expr::InList` の腕がないだけ。

すでに `Expr::Between` と `Expr::Like`、`is_checked_select_comparison_operator` の腕は
両方で**同じ関数を共有している**。同じように `render_checked_in_list()` を呼ぶ腕を足す。

## 先に計測すること

`SELECT` 側で計測済みの内容（`../COMPAT.md` の `IN` の節に記録されている）がそのまま効くはずだが、
**DML でも同じかを必ず確かめる**。特に `UPDATE` の影響行数。

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20));
INSERT INTO t VALUES (1,'b'),(2,'A'),(3,'c');

UPDATE t SET name = 'z' WHERE name IN ('a','C');   -- 何行 affected か
SELECT id, name FROM t;
DELETE FROM t WHERE id IN (1, NULL);               -- 何行 affected か
SELECT id FROM t;
DELETE FROM t WHERE id NOT IN (2, NULL);           -- 0 行のはず
```

`UPDATE` の affected rows は `CLIENT_FOUND_ROWS` の有無で意味が変わる
（`../COMPAT.md` の該当節を読むこと）。

## 実装

`mysql/parser/translate.rs` の `render_dml_predicate()` の `match` に足す:

```rust
Expr::InList {
    expr,
    list,
    negated,
} => render_checked_in_list(expr, list, *negated, render_context),
```

`render_checked_in_list()` は `SelectRenderContext` を取り、
`checked_comparisons` に記録する。DML 側も同じ context を使っているので
（`parse_dml()` が `SelectRenderContext::new(sql, &[])` を作っている）、
記録はそのまま `TranslatedDml.checked_comparisons` に載る。

**注意点が 1 つある。** DML の経路は `SelectRenderContext::new(sql, &[])` を
**テキスト列リストなしで**作っている。つまり `is_text_column()` は常に false を返し、
`?` を含む `IN` リストは照合されない。テキスト列に対する `WHERE s IN (?)` が
`SELECT` 側と違う結果になりうる。

- テキストリテラルを含むリストは、リテラル自身から `collated` が立つので問題ない。
- `?` だけのリストは照合されない。**この差を確認し**、
  問題になるなら DML でもプレースホルダを拒否するか、`../TODO.md` に 1 行残す。

## テスト

- パーサ（`mysql/parser/tests.rs`）: 現在 `"DELETE FROM t WHERE value IN (1, 2)"` が
  拒否リストに入っている。**そこから外し**、翻訳後 SQL を確認するテストに置き換える。
  `UPDATE` 側も同じく。
- エンドツーエンド（`mysql/server/src/frontend_adapter/tests.rs`）:
  `DELETE ... WHERE id IN (...)` で**残った行**と**affected rows** を確認する。
  `UPDATE ... WHERE name IN ('a','C')` で、大文字小文字を無視して 2 行更新されること。
  `NOT IN` に NULL を含む場合に 0 行であること。

## ドキュメント

- `mysql/TODO.md` の SELECT 表から
  「`IN` over a list of values in `UPDATE` / `DELETE`」の行を消す。
- `mysql/COMPAT.md` の `IN` の節に、DML でも同じ規則が効くことを 1 文足す。
  プレースホルダの扱いが違うならそれも書く。
