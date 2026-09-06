# task012: 修飾された列との比較 `WHERE t.id = 1` を通す

**難易度: 中** / 参照: `../TODO.md` 「SQL syntax → SELECT」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`WHERE t.id = 1` のように、比較の左辺が `テーブル.列` の形で書かれているもの。
現在は `mysql/parser/translate.rs` の `render_checked_select_comparison()` が
`Expr::Identifier` しか受けず、`unsupported("SELECT comparison requires one unqualified column")`
で落ちる。

**これは CTE を実用にするための鍵。** `WITH c AS (...) SELECT c.n FROM c WHERE c.id = 1` が
今は書けない。COMPAT.md にもそう書いてある。

## 難所

比較は「値が列の型に収まるか」を後段で検査している。その検査は
`mysql/frontend/session.rs` の `validate_select_comparison_columns()` にあり、
**`source_table` という 1 つのテーブルに対してしか列を引けない。**
`CheckedSelectComparison` は `column_name` しか持っていないので、
修飾子を渡す口がない。

## スコープの決め方

いきなり JOIN まで通そうとしないこと。JOIN 中の `WHERE` 比較は
`../TODO.md` に別行で載っている、より広い課題。

**このタスクのスコープ:** 修飾子が「この文の唯一のソース」を指している場合だけ受ける。
唯一のソースとは、素のテーブル名、その別名、または CTE 名。
ソースが 2 つ以上（JOIN、UNION）の文で修飾された比較が来たら、これまでどおり拒否する。

これだけで `WHERE t.id = 1`（別名付きの単一テーブル）と
`WHERE c.id = 1`（CTE）の両方が通る。

## 実装

1. `mysql/parser/lib.rs`
   - `CheckedSelectComparison` に `qualifier: Option<String>` を足す。
     アクセサ `qualifier()` も足す。
2. `mysql/parser/translate.rs`
   - `render_checked_select_comparison()` で
     `Expr::CompoundIdentifier(parts) if parts.len() == 2` を左右どちらでも受ける。
     描画は `"表"."列"`（`render_ident` を 2 つ繋ぐ。`render_join_column()` が同じことをしている）。
   - `render_context.is_text_column(&column_name)` は列名だけで引いている。
     単一ソース前提なのでそのままでよいが、その前提をコメントに書く。
   - `render_checked_between()` と `render_checked_like()`、`render_checked_in_list()` も
     同じ理由で `Expr::Identifier` に限っている。**同じ修正を全部に入れるか、
     このタスクでは `=` 系だけにして残りを `../TODO.md` に残すかを決める。**
     一度に全部やるほうが一貫するが、差分は大きくなる。
3. `translate_select_query()`
   - `source_tables` が確定した後で、記録した各比較の `qualifier` を検査する。
     修飾子が「唯一の非サブクエリ・ソースの `reference`」と
     大文字小文字を無視して一致しなければ `unsupported(...)` で拒否する。
     ソースが 2 つ以上あるなら修飾された比較はすべて拒否する。
   - `MySqlSelectSource` の `reference` が別名／CTE 名を持っている
     （`mysql/parser/translate.rs` の `render_select_table()` と
     `render_common_table_expressions()` を読むこと）。
4. `mysql/frontend/session.rs`
   - `validate_select_comparison_columns()` は `source_table` 1 つで列を引いている。
     修飾子が唯一のソースを指すことをパーサ側で保証したので、**この関数は変更不要**のはず。
     変更不要であることを、CTE を含むテストで実際に確かめること。
   - CTE の場合、`source_table` が CTE の基底テーブルになっているか、
     CTE が射影で列を並べ替えていても正しい列に解決されるかを必ず確認する。
     `MySqlSelectSource` の `projected_columns` がその解決に使われている
     （過去に、この解決を飛ばして列メタデータが入れ替わるバグが実際に出ている）。

## テスト

- パーサ: `SELECT id FROM users u WHERE u.id = 1` の翻訳後 SQL が
  `... WHERE ("u"."id" = 1)`。修飾子が存在しない別名を指す場合の拒否。
  JOIN 中の修飾された比較が拒否されたままであること。
- エンドツーエンド（`mysql/server/src/frontend_adapter/tests.rs`）:
  - 別名付き単一テーブルで行が正しく絞られること
  - **CTE で `WHERE c.id = 1` が動くこと。**
    `WITH c AS (SELECT n, id FROM f) SELECT c.n, c.id FROM c WHERE c.id = 1` のように
    **CTE が射影の順序を入れ替えている**形を必ず入れる。ここが過去にバグった箇所。
  - テキスト列に対する修飾された比較で `COLLATE NOCASE` が効くこと
  - 型が合わない値（テキスト列に整数など）が従来どおり拒否されること

## ドキュメント

- `mysql/TODO.md` の SELECT 表から「Comparison against a qualified column」の行を消す。
  `BETWEEN` / `LIKE` / `IN` を落とすなら、それぞれ 1 行残す。
- `mysql/COMPAT.md` の CTE の節に「`WHERE c.id = 1` は修飾された比較の制限で拒否される」と
  書いてある箇所を書き換える。
