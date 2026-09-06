# task016: 括弧付き `UNION` 枝を通す

**難易度: 低** / 参照: `../TODO.md` 「SQL syntax → SELECT」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`(SELECT id FROM a) UNION (SELECT id FROM b)` — 各枝が括弧で囲まれた形。
多くのクライアントとツールがこの形を出す。

現在は `mysql/parser/translate.rs` の `translate_select_query()` で
`SetExpr::SetOperation` の左右を

```rust
let (SetExpr::Select(left), SetExpr::Select(right)) = (left.as_ref(), right.as_ref())
else {
    return unsupported("SELECT UNION branch");
};
```

と直接 `SetExpr::Select` に決め打ちしているため、
括弧が付くと `SetExpr::Query` になって落ちる。

## 先に計測すること

```sql
CREATE TABLE a (id INT NOT NULL PRIMARY KEY);
CREATE TABLE b (id INT NOT NULL PRIMARY KEY);
INSERT INTO a VALUES (1),(2);
INSERT INTO b VALUES (2),(3);

(SELECT id FROM a) UNION (SELECT id FROM b);
(SELECT id FROM a) UNION ALL (SELECT id FROM b);
(SELECT id FROM a) UNION (SELECT id FROM b) ORDER BY id;
(SELECT id FROM a ORDER BY id LIMIT 1) UNION (SELECT id FROM b);  -- 枝が自前の ORDER/LIMIT を持つ
```

`--column-type-info` で結果列を記録し、**括弧なしの形と同じメタデータになるか**を確認する。
同じなら実装は素直。

**枝が自前の `ORDER BY` / `LIMIT` を持つ形は別問題。** 最初のスライスでは拒否する。

## 実装

`mysql/parser/translate.rs` の `translate_select_query()`:

1. 括弧を剥がすヘルパを書く。`SetExpr::Query(query)` のとき、
   その内側の `Query` が「`SELECT` 本体だけを持ち、`with` / `order_by` /
   `limit_clause` / `fetch` / `locks` などをいっさい持たない」場合に限り、
   内側の `body` を返す。持っていたら `None` を返して拒否する。
   ここを緩めると、枝の `LIMIT` を黙って捨てることになる。**必ず拒否すること。**
2. `SetExpr::Select` と、剥がした結果の `SetExpr::Select` を同じに扱う。
3. すでに追加されている `ordered_projection`（`ORDER BY` 序数のための射影参照）も
   同じヘルパを通す必要がある。**片方だけ直すと、
   括弧付き UNION に序数を書いたとき射影が空になって静かに壊れる。**

`sqlparser` が括弧をどの AST で表すか（`SetExpr::Query` か別のものか）を
実際に確かめてから書くこと。次のような小さなテストで確認できる。

```rust
let statements = sqlparser::parser::Parser::parse_sql(
    &SessionMySqlDialect::default(),
    "(SELECT id FROM a) UNION (SELECT id FROM b)",
).unwrap();
println!("{statements:#?}");
```

## テスト

- パーサ: 括弧あり／なしの `UNION` が**同じ翻訳後 SQL**になること。
  `UNION ALL` も。枝が `ORDER BY` / `LIMIT` / `WITH` を持つ形が拒否されること。
  括弧付き UNION に `ORDER BY 1` を足した形が正しく解決されること。
- エンドツーエンド: 行と結果列メタデータが括弧なしの形と一致すること。

## ドキュメント

- `mysql/TODO.md` の SELECT 表から「A parenthesised `UNION` branch」の行を消す。
  枝の `ORDER BY` / `LIMIT` を落とすなら 1 行残す。
- `mysql/COMPAT.md` の UNION の節（「a left side that is not one ...」と書いてある箇所）を
  書き換える。
