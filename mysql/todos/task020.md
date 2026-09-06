# task020: `ORDER BY` / `LIMIT` 付きの `UPDATE` / `DELETE` を通す

**難易度: 中** / 参照: `../TODO.md` 「SQL syntax → DML」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`DELETE FROM t WHERE x = 1 ORDER BY id LIMIT 10` と、その `UPDATE` 版。
バッチ削除・バッチ更新でよく使われる形。

現在は `mysql/parser/translate.rs` の `translate_update()` / `translate_delete()` が
`ORDER BY` や `LIMIT` の付いた文をまとめて拒否している。

## 難所

**エンジンが `DELETE ... ORDER BY ... LIMIT` を受けるとは限らない。**
SQLite は `SQLITE_ENABLE_UPDATE_DELETE_LIMIT` を有効にしたビルドでしか受けない。
**着手して最初にやることは、エンジンが受けるかを確かめること。**

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER);
INSERT INTO t VALUES (1,10),(2,20),(3,30);
DELETE FROM t WHERE n > 5 ORDER BY id LIMIT 1;
SELECT id FROM t;
SQL
```

受けないなら、この形は**素直には通せない**。副問い合わせに書き換える手はある:

```sql
DELETE FROM t WHERE id IN (SELECT id FROM t WHERE n > 5 ORDER BY id LIMIT 1)
```

ただしこれは主キーが 1 列であることに依存する。
**書き換えを選ぶなら、主キーが単一列であることを検査し、
そうでないテーブルは拒否すること。** 検査を飛ばすと、
主キーが複合のテーブルで黙って間違った行を消すことになる。

エンジンが受けないうえに書き換えも安全に書けないなら、
**このタスクは「エンジン側に `DELETE ... LIMIT` を足す」という別の作業に化ける。**
その場合は調査結果を `../TODO.md` に書いて、ここでは止めること。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT);
INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40);

DELETE FROM t WHERE n > 5 ORDER BY id LIMIT 2;
SELECT id FROM t;                          -- どの 2 行が消えたか
DELETE FROM t ORDER BY id DESC LIMIT 1;
SELECT id FROM t;
UPDATE t SET n = 0 ORDER BY id LIMIT 1;
SELECT id, n FROM t;
DELETE FROM t LIMIT 1;                     -- ORDER BY なしの LIMIT
DELETE FROM t ORDER BY id;                 -- LIMIT なしの ORDER BY
```

記録すること:

- 各文の **affected rows**
- **どの行が影響を受けたか。** `ORDER BY` なしの `LIMIT` は順序が未定義なので、
  MySQL の答えを「正しい順序」として固定してはいけない。
  **`ORDER BY` なしの `LIMIT` は拒否する**のが安全。拒否するなら 1 行 `../TODO.md` に残す。
- `LIMIT` に `OFFSET` を付けられるか（MySQL の `DELETE` は `LIMIT n` のみで
  `OFFSET` を取らないはず。実測で確認）

## 実装

1. `mysql/parser/translate.rs`
   - `translate_delete()` / `translate_update()` が `ORDER BY` / `LIMIT` を
     拒否している箇所を緩める。
   - `ORDER BY` の描画は `render_select_order_by()` を使い回せるか検討する。
     ただしあれは `SelectItem` の射影を引数に取る（序数解決のため）。
     DML には射影がないので、**序数は拒否**して呼ぶ形にする。
     テキスト列の `COLLATE NOCASE` は DML でも必要。
     ただし DML 経路は `SelectRenderContext::new(sql, &[])` を
     テキスト列リストなしで作っているので、**照合が効かない。**
     `TranslatedDml` 側にも 2 回目のレンダリングの仕組みが要るかを判断する。
     要るなら、それはこのタスクより大きい。**その場合はテキスト列に対する
     `ORDER BY` を拒否し、整数列だけ受けること。**
   - `LIMIT` は `render_select_limit()` / `render_select_row_count()` を使い回せる。
     `OFFSET` は MySQL が取らないなら拒否する。
2. 影響行数の数え方
   - `mysql/frontend/session.rs` の `affected_rows()` 周辺。
     `LIMIT` で絞られたときも同じ数え方でよいかを確かめる。
     `CLIENT_FOUND_ROWS` の扱いも `../COMPAT.md` の該当節で確認する。

## テスト

- パーサ: `ORDER BY` + `LIMIT` 付きの `DELETE` / `UPDATE` の翻訳後 SQL。
  `ORDER BY` なしの `LIMIT`、`OFFSET` 付き、`ORDER BY` の序数が
  （方針どおりに）拒否されること。
  現在 `mysql/parser/tests.rs` の拒否リストに
  `"UPDATE t SET value = 1 ORDER BY value"` と
  `"DELETE FROM numbers ORDER BY id"` が入っているので、そこを整理する。
- エンドツーエンド: **どの行が残ったか**を必ず確認する。
  affected rows も。データは順序が判別できるものにすること
  （4 行入れて `LIMIT 2` で消すなど）。

## ドキュメント

- `mysql/TODO.md` の DML 表から
  「`ORDER BY` or `LIMIT` on an `UPDATE` / `DELETE`」の行を消す。
  落とした形（`ORDER BY` なしの `LIMIT`、テキスト列の `ORDER BY` など）は 1 行残す。
- `mysql/COMPAT.md` の DML の節に書く。エンジンに書き換えを入れたなら、
  その書き換えと主キー単一列の前提を明記する。
