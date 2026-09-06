# task015: `CROSS JOIN` を通す

**難易度: 中** / 参照: `../TODO.md` 「SQL syntax → SELECT」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`SELECT a.id, b.id FROM a CROSS JOIN b` — 条件なしの直積。
`INNER JOIN` / `LEFT JOIN` / `RIGHT JOIN` はすでに通っている。

現在は `mysql/parser/translate.rs` の `checked_join()` が
`ON` 句を持つ結合しか受けないため落ちる。

`../TODO.md` には `CROSS JOIN`、`USING`、カンマ結合が 1 行にまとめて載っているが、
**このタスクは `CROSS JOIN` だけ**を扱う。`USING` は結合列の解決が別問題、
カンマ結合は `render_select_body()` が「複数の table source」として別経路で
拒否しているので、それぞれ別タスクにする。

## 先に計測すること

```sql
CREATE TABLE a (id INT NOT NULL PRIMARY KEY, x VARCHAR(10));
CREATE TABLE b (id INT NOT NULL PRIMARY KEY, y VARCHAR(10));
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (10,'r'),(20,'s');

SELECT a.id, b.id FROM a CROSS JOIN b ORDER BY a.id, b.id;
SELECT a.x, b.y FROM a CROSS JOIN b ORDER BY a.id, b.id;
SELECT COUNT(*) FROM a CROSS JOIN b;
```

`--column-type-info` で結果列を記録する。要点:

- **`NOT_NULL` フラグ。** `CROSS JOIN` はどちらの側も欠けないので、
  `LEFT JOIN` と違い `NOT NULL` 列は `NOT NULL` のままのはず。実測で確認。
- 結果列の `table` / `org_table` がどう報告されるか（既存の JOIN テストが
  同種のことを確かめているので、そこに合わせる）

## 実装

1. `mysql/parser/translate.rs` の `checked_join()`
   - `sqlparser::ast::JoinOperator::CrossJoin` を受ける。
     **これは `CrossJoin(JoinConstraint)` という形で `JoinConstraint` を 1 つ持つ**
     （sqlparser 0.62.0 の `src/ast/query.rs`）。素の `CROSS JOIN` なら
     `JoinConstraint::None` のはずだが、実際に何が入るかをデバッグ出力で確認し、
     `None` 以外は拒否すること。
   - 既存のシグネチャは `(keyword, constraint)` を返している。
     `CROSS JOIN` には `ON` がないので、戻り値の形を変えるか、
     `Option<&Expr>` にして呼び出し側で分岐する。
2. `render_select_body()` の結合ループ
   - `ON` がない場合に `" ON "` と述語を足さないようにする。
   - `keyword` は `"CROSS JOIN"`。エンジンが `CROSS JOIN` を受けるかを
     `tursodb` で必ず確かめる。受けないなら `,` か `JOIN ... ON 1=1` に落とす。
   - **`source.outer` は立てない。** `LEFT JOIN` / `RIGHT JOIN` だけが
     片側の `NOT NULL` を落とす。ここを間違えると
     結果列のフラグが MySQL と食い違う。
3. `reject_unqualified_join_projection()` はそのまま効く。
   `CROSS JOIN` でも射影は修飾されている必要がある。
4. `mysql/server/src/frontend_adapter.rs` の権限・カタログ検査
   - JOIN のすべてのテーブルを認可し、内部カタログテーブルを弾く経路は
     すでに「全テーブルを見る」ように直っている。`CROSS JOIN` でも
     `source_tables` に両方が載ることを確かめる。

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
CREATE TABLE a (id INTEGER PRIMARY KEY, x TEXT);
CREATE TABLE b (id INTEGER PRIMARY KEY, y TEXT);
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (10,'r'),(20,'s');
SELECT a.id, b.id FROM a CROSS JOIN b ORDER BY a.id, b.id;
SQL
```

## テスト

- パーサ: `FROM a CROSS JOIN b` の翻訳後 SQL。`source_tables` に 2 つ載ること。
  修飾されていない射影が拒否されること。
- エンドツーエンド: **4 行（2×2）返ること**、
  **`NOT NULL` 列のフラグが落ちていないこと**（`LEFT JOIN` との違いをテストで対比させる）、
  両テーブルが認可されること。

## ドキュメント

- `mysql/TODO.md` の SELECT 表の
  「`CROSS JOIN`, `USING`, the comma join」の行から `CROSS JOIN` を外し、
  残り 2 つの行として書き直す。
- `mysql/COMPAT.md` の JOIN の節に 1 文足す。
