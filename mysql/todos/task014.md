# task014: ワイルドカード射影に対する `ORDER BY <序数>` を通す

**難易度: 中** / 参照: `../TODO.md` 「SQL syntax → SELECT」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`SELECT * FROM t ORDER BY 2` — 序数が指す列がワイルドカードの展開後にしか分からない形。

序数そのものはすでに通っている（`mysql/parser/translate.rs` の
`order_by_ordinal()` と `projected_expr()`）。`projected_expr()` が
`SelectItem::Wildcard` を見つけると `unsupported(...)` を返しているのが、今の制限。

## なぜ中難易度か

パーサはテーブルの列を知らない。列の並びを知っているのはフロントエンド側
（`mysql/frontend/session/catalog.rs` の `list_columns()`）だけ。
つまり、**列の型を知ってから 2 回目のレンダリングをする既存の仕組みに乗せる**か、
序数を解決しないままエンジンに渡すかのどちらかになる。

### 案 A（推奨）: 2 回目のレンダリングに乗せる

`TranslatedSelect::needs_column_types()` が真のとき、フロントエンドは
`parse_select_with_text_columns(sql, mode, &text_columns)` で 2 回目のレンダリングをする。
`SelectRenderContext` はそのときテキスト列名のリストを持つ。

**しかしこれは「どの列がテキストか」しか渡していない。序数の解決には
「列がどの順に並んでいるか」が要る。** 仕組みを 1 段広げる必要がある:

- `SelectRenderContext` に「射影の列名を順番どおりに並べたリスト」を渡せるようにする
- `mysql/frontend/session.rs` の `parse_select_knowing_column_types()` が
  そのリストを `list_columns()` の結果から作って渡す

### 案 B: 序数をそのままエンジンに渡す

エンジン（SQLite 系）も `ORDER BY 2` を位置参照として解釈する。
値の順序だけなら合う。**しかし照合順序が合わない。**
テキスト列を序数で並べたとき、MySQL は大文字小文字を無視し、
エンジンはバイト順で並べる。`ORDER BY 2` と `ORDER BY name` で結果が変わってしまう。

案 B を採るなら、**この差異を COMPAT.md に明記する**こと。
既存の実装が名前付き `ORDER BY` でわざわざ `COLLATE NOCASE` を付けている理由が
そこにあるので、黙って崩さないこと。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), n INT);
INSERT INTO t VALUES (1,'b',10),(2,'A',30),(3,'c',20);

SELECT * FROM t ORDER BY 2;        -- name で大文字小文字を無視して並ぶか
SELECT * FROM t ORDER BY 2 DESC;
SELECT * FROM t ORDER BY 4;        -- 範囲外（1054 のはず）
SELECT t.*, id FROM t ORDER BY 4;  -- ワイルドカードと明示列の混在
SELECT * FROM t ORDER BY 1, 2;
```

`SELECT * FROM t ORDER BY 2` が `2, 1, 3` の順（`A`, `b`, `c`）になることを確認する。

## 実装

案 A を採る場合の骨子:

1. `mysql/parser/translate.rs`
   - `SelectRenderContext` に射影列名のリストを持たせる。
   - `projected_expr()` が `SelectItem::Wildcard` に当たったとき、
     そのリストを使って序数から列名を求め、`Expr::Identifier` を組み立てて返す。
     借用の都合で `&Expr` を返せなくなるので、戻り値を
     `Cow<'_, Expr>` や `Option<Ident>` に変える必要がある。
   - ワイルドカードと明示列の混在（`SELECT t.*, id`）は、
     ワイルドカードが何列に展開されるかを知らないと数えられない。
     **最初のスライスでは、射影がワイルドカード 1 つだけの場合に限る**のが安全。
     混在は拒否したまま `../TODO.md` に 1 行残す。
2. `mysql/parser/lib.rs`
   - `TranslatedSelect::needs_column_types()` が、序数を含みワイルドカード射影の文で
     真になるようにする。`orders_a_bare_column` に相当するフラグを増やす。
   - `parse_select_with_text_columns()` のシグネチャに射影列名リストを足す
     （引数が増えるので、既存の呼び出し元をすべて直す）。
3. `mysql/frontend/session.rs`
   - `parse_select_knowing_column_types()` が `list_columns()` の結果から
     列名を順番どおりに取り出して渡す。
   - **`list_columns()` の並びがテーブルの宣言順であることを確認する。**
     ここが違うと静かに間違った列で並べ替えることになる。

## テスト

- パーサ: 射影列名リストを渡したときに `SELECT * ... ORDER BY 2` が
  `... ORDER BY "name" COLLATE NOCASE ASC` に落ちること。
  リストを渡さない 1 回目は `needs_column_types()` が真になること。
  範囲外の序数、ワイルドカードと明示列の混在が拒否されること。
- エンドツーエンド: **`'A'` と `'b'` を含むデータ**で
  `SELECT * FROM t ORDER BY 2` と `SELECT * FROM t ORDER BY name` が同じ順になること。

## ドキュメント

- `mysql/TODO.md` の SELECT 表から
  「`ORDER BY` an ordinal over a wildcard projection」の行を消す。
  混在を落とすなら 1 行残す。
- `mysql/COMPAT.md` の `ORDER BY` の節（序数について書いてある箇所）を書き換える。
