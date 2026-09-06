# task011: `<=>`（NULL 安全等価）を通す

**難易度: 低** / 参照: `../TODO.md` 「SQL syntax → SELECT」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`a <=> b` — MySQL の NULL 安全等価。`=` と違い、両辺が NULL なら真、片方だけ NULL なら偽。
決して NULL を返さない。

SQLite 系の `IS` / `IS NOT` が同じ意味を持つ。エンジンが `a IS b` を受けるかを
`tursodb` で必ず確かめる。受けないなら `(a IS NULL AND b IS NULL) OR a = b` に展開する。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT, s VARCHAR(20));
INSERT INTO t VALUES (1, 1, 'a'), (2, NULL, NULL);

SELECT id FROM t WHERE n <=> 1;
SELECT id FROM t WHERE n <=> NULL;
SELECT id FROM t WHERE n <=> 2;
SELECT id FROM t WHERE s <=> 'A';     -- 照合順序が効くか
SELECT id FROM t WHERE NOT (n <=> 1);
SELECT 1 <=> 1, NULL <=> NULL, 1 <=> NULL;
```

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT);
INSERT INTO t VALUES (1,1,'a'),(2,NULL,NULL);
SELECT id FROM t WHERE n IS 1;
SELECT id FROM t WHERE n IS NULL;
SELECT id FROM t WHERE (s COLLATE NOCASE) IS 'A';
SQL
```

**照合順序の確認を飛ばさないこと。** `=` の側はすでに
`render_checked_select_comparison()` で `COLLATE NOCASE` を付けている。
`<=>` も同じ扱いにしないと、`s = 'A'` と `s <=> 'A'` で結果が変わってしまう。

## 実装

`mysql/parser/translate.rs` の `render_checked_select_comparison()` が
そのまま使える形になっているかを見る。

1. `sqlparser` が `<=>` をどう表すかを確認する。`BinaryOperator::Spaceship` のはず。
   確認したうえで:
   - `is_checked_select_comparison_operator()` に足す
   - `checked_select_comparison_operator()`（`BinaryOperator` → `CheckedSelectComparisonOperator`）
     に足す。`CheckedSelectComparisonOperator` に新 variant（例: `NullSafeEqual`）が要る。
     この enum は `mysql/parser/lib.rs` にある。
   - `checked_select_comparison_sql_operator()`（`BinaryOperator` → エンジンの演算子文字列）
     で `IS` を返す
   - `reverse_checked_comparison_operator()` に足す（`<=>` は対称なので自分自身を返す）
2. **`CheckedSelectComparisonRhs::Null` の扱いを確認する。**
   `mysql/frontend/session.rs` の `checked_comparison_fits_column()` は
   NULL を整数列・テキスト列のどちらでも通している。`<=>` でもそのままでよい。
3. `?` プレースホルダとの組み合わせも既存の経路に乗る。
   `n <=> ?` に NULL を束縛したときに正しく動くかをエンドツーエンドで確かめる。

## テスト

- パーサ: `WHERE n <=> 1` → `WHERE ("n" IS 1)`、`WHERE n <=> NULL` → `WHERE ("n" IS NULL)`。
  テキスト列なら `COLLATE NOCASE` が付くこと（2 回目のレンダリング経路）。
  `checked_comparisons()` に演算子が記録されること。
- エンドツーエンド: **NULL を含む行**を必ず入れ、`=` と `<=>` で結果が変わることを確認する。
  `n = NULL` は 0 行、`n <=> NULL` は 1 行。プレースホルダ版も見る。

## ドキュメント

- `mysql/TODO.md` の SELECT 表から `` `<=>` `` の行を消す。
- 食い違いが残るなら `mysql/COMPAT.md` の比較の節に書く。
