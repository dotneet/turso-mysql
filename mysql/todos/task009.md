# task009: `COUNT(DISTINCT col)` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Not looked at → Aggregates」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`COUNT(DISTINCT col)` — 重複を除いた個数。`COUNT(col)` と `COUNT(*)` はすでに通っている。

**なぜ今は落ちるか:** `mysql/parser/static_select_metadata.rs` の `is_plain_aggregate()` が
`arguments.duplicate_treatment.is_none()` を要求しているため、`DISTINCT` が付いた集約は
その時点で `None` になる。ここを緩めるのがこのタスクの中心。

エンジンは `count(DISTINCT x)` を素直に受けるはずだが、必ず `tursodb` で確かめること。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, team VARCHAR(20), n INT);
INSERT INTO t VALUES (1,'a',1),(2,'a',2),(3,'b',2),(4,'B',NULL);

SELECT COUNT(DISTINCT team) FROM t;   -- 'a','b','B' → 照合順序が効くか（2 か 3 か）
SELECT COUNT(DISTINCT n) FROM t;      -- NULL は数えないはず
SELECT COUNT(DISTINCT team, n) FROM t; -- 複数引数版（落とすなら記録だけ）
SELECT team, COUNT(DISTINCT n) FROM t GROUP BY team;
```

`--column-type-info` で結果列を記録する。**`COUNT(col)` と同じ型・長さ・フラグかどうか**が要点。
同じなら実装は分岐を足さずに済む。

**結果列の名前にも注意。** MySQL は式に別名がなければソーステキストで名前を付ける。
`COUNT(DISTINCT team)` の列名が `COUNT(DISTINCT team)` になるか、
空白の入れ方まで含めてそのまま返るかを実測する。
`mysql/parser/translate.rs` の `mysql_aggregate_column_name()` と `source_text()` が
その担当なので、そこを直す必要があるかを判断する。

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
CREATE TABLE t (id INTEGER PRIMARY KEY, team TEXT, n INTEGER);
INSERT INTO t VALUES (1,'a',1),(2,'a',2),(3,'b',2),(4,'B',NULL);
SELECT count(DISTINCT team), count(DISTINCT n) FROM t;
SELECT count(DISTINCT team COLLATE NOCASE) FROM t;
SQL
```

**照合順序が核心。** MySQL の既定照合順序は大文字小文字を無視するので
`COUNT(DISTINCT team)` は `'b'` と `'B'` を 1 つに数える可能性が高い。
エンジンは区別する。既存の `ORDER BY` や `WHERE` 比較が
`COLLATE NOCASE` で揃えているのと同じ手を使えるか、上のクエリで確かめる。
使えないなら、**テキスト列に対する `COUNT(DISTINCT ...)` は拒否**して数値列だけ受ける。
その場合は `../TODO.md` に 1 行残し、理由を書く。

## 実装

1. `mysql/parser/static_select_metadata.rs`
   - `is_plain_aggregate()` は他の集約からも呼ばれている。**そのまま緩めると
     `SUM(DISTINCT x)` などまで通ってしまう**ので、`COUNT` の経路だけが
     `DISTINCT` を許すようにする。`is_count_call()` の側で扱うのが素直。
   - `StaticSelectMetadata` に「DISTINCT 付きの COUNT」であることが伝わるようにする。
     結果列のメタデータが `COUNT(col)` と同じなら、既存の COUNT の形をそのまま返してよい。
2. `mysql/parser/translate.rs`
   - `render_aggregate_call()` / `aggregate_argument()` が `DISTINCT` を落としている。
     `count(DISTINCT "team")` を出せるようにする。
     照合順序を挟むなら `count(DISTINCT "team" COLLATE NOCASE)` の形になる。
   - `mysql_aggregate_column_name()` で列名が計測どおりになることを確認する。
   - `render_select_order_by()` の `is_count_call` を通る経路にも影響しうる。
     `ORDER BY COUNT(DISTINCT x)` を試して壊れないことを見る。
3. `mysql/server/src/frontend_adapter.rs`
   - `COUNT(col)` と同じ形でよければ変更なし。違うなら `aggregate_column_definition()` を直す。

## テスト

- パーサ: `SELECT COUNT(DISTINCT team) FROM t` の翻訳後 SQL と結果列名。
  `SUM(DISTINCT n)` など、通すつもりのない `DISTINCT` 付き集約が拒否されたままであること。
- エンドツーエンド: **`'b'` と `'B'` を含むデータ**で数え上げが MySQL と一致すること。
  `GROUP BY` と併用した場合も見る。結果列の型・長さ・フラグ。

## ドキュメント

- `mysql/TODO.md` の「Aggregates」行から `COUNT(DISTINCT ...)` を消す。
- 照合順序で食い違うか、テキスト列を落とすなら `mysql/COMPAT.md` に書く。
