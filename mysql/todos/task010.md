# task010: `GROUP_CONCAT` を通す

**難易度: 中** / 参照: `../TODO.md` 「Functions → Not looked at → Aggregates」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`GROUP_CONCAT(col)` — グループ内の値を連結した文字列を返す集約。
エンジンにも `group_concat` がある（`core/function.rs`）。

**中難易度である理由が 3 つある。**

1. **既定の区切り文字が違う可能性がある。** MySQL は `,`（カンマのみ）、
   SQLite 系は `,`（カンマのみ）のことが多いが、**必ず計測して突き合わせる。**
2. **順序が保証されない。** MySQL は `GROUP_CONCAT(col ORDER BY col)` を書ける。
   エンジンにその構文があるとは限らない。最初のスライスでは `ORDER BY` 付きを拒否する。
3. **結果の幅。** MySQL は `group_concat_max_len`（既定 1024）で切り詰める。
   結果列の `column_length` がその値に由来するのか、列幅に由来するのかを実測する。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, team VARCHAR(20), name VARCHAR(30));
INSERT INTO t VALUES (1,'a','x'),(2,'a','y'),(3,'b','z'),(4,'a',NULL);

SELECT GROUP_CONCAT(name) FROM t;
SELECT team, GROUP_CONCAT(name) FROM t GROUP BY team;   -- NULL が飛ばされるか
SELECT GROUP_CONCAT(name SEPARATOR '-') FROM t;          -- 区切り指定
SELECT GROUP_CONCAT(DISTINCT name) FROM t;
SELECT GROUP_CONCAT(name ORDER BY name DESC) FROM t;
SELECT @@group_concat_max_len;
```

`--column-type-info` で記録する。

- 結果の `column_type`（BLOB 252 になる可能性がある。VAR_STRING ではないかもしれない）
- **`column_length`。** `group_concat_max_len` に一致するか、列幅の倍数か。
- `NOT_NULL` フラグ（グループが空なら NULL なので落ちるはず）
- 既定の区切り文字と、NULL 値の扱い

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
CREATE TABLE t (id INTEGER PRIMARY KEY, team TEXT, name TEXT);
INSERT INTO t VALUES (1,'a','x'),(2,'a','y'),(3,'b','z'),(4,'a',NULL);
SELECT group_concat(name) FROM t;
SELECT group_concat(name, '-') FROM t;
SELECT team, group_concat(name) FROM t GROUP BY team;
SQL
```

**区切り文字の指定方法が構文レベルで違う**（MySQL は `SEPARATOR 'x'`、
エンジンは第 2 引数）ので、翻訳で吸収する必要がある。
`sqlparser` が `SEPARATOR` をどの AST に載せるかを実際に確かめてから実装すること。
最初のスライスでは `SEPARATOR` なしだけを受け、指定付きは拒否してよい。

## 実装

1. `mysql/parser/static_select_metadata.rs`
   - `column_aggregate_argument()` と `ColumnAggregateKind` が `MIN`/`MAX`/`SUM`/`AVG` を
     扱っている。`GROUP_CONCAT` は結果がテキストなので、この enum に足すか、
     `ScalarCall` 側ではなく新しい種別として扱うかを設計する。
     **既存の `ColumnAggregateKind` に足すのが素直**（例: `Concatenated`）。
   - `is_plain_aggregate()` が `arguments.clauses.is_empty()` を要求している。
     `SEPARATOR` や `ORDER BY` はここに載る可能性が高いので、
     拒否したままにするならこの条件をそのまま使えばよい。
2. `mysql/parser/translate.rs`
   - `render_aggregate_call()` が `function.name` をそのまま出している。
     `GROUP_CONCAT` → `group_concat` に落ちるかを確認する（大文字のままでも
     エンジンが受けるかは要確認）。
   - 結果列の名前が計測どおりになるか `mysql_aggregate_column_name()` で確認。
3. `mysql/server/src/frontend_adapter.rs`
   - `aggregate_column_definition()` に分岐を足し、計測した型・長さ・フラグを組み立てる。
   - `MIN`/`MAX` がテキスト列で `column_length` をどう出しているかが参考になる
     （COMPAT.md に「`MIN` over a `TEXT` column は列幅を返すが MySQL は 1048560 を返す」
     という既知の差異が書いてある。同種の問題がここでも起きる）。

## テスト

- パーサ: `SELECT GROUP_CONCAT(name) FROM t` の翻訳後 SQL と結果列名。
  `SEPARATOR` 付き・`ORDER BY` 付き・`DISTINCT` 付きが拒否されること。
- エンドツーエンド: `GROUP BY` 併用で**グループごとの連結結果**が MySQL と一致すること。
  NULL を含む行を必ず入れる。結果列の型・長さ・フラグ。

## ドキュメント

- `mysql/TODO.md` の「Aggregates」行から `GROUP_CONCAT` を消す。
  `SEPARATOR` / `ORDER BY` / `DISTINCT` を落とすなら 1 行残す。
- 区切り文字・順序・切り詰め長で食い違うなら `mysql/COMPAT.md` に書く。
