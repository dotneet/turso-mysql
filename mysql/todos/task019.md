# task019: `TINYTEXT` / `MEDIUMTEXT` / `LONGTEXT` と `BLOB` 各サイズを通す

**難易度: 中** / 参照: `../TODO.md` 「Column types」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`TEXT` と `BLOB` はすでに通っている。同じ系列の他のサイズが通っていない:

- `TINYTEXT`（255 バイト）、`MEDIUMTEXT`（16MB）、`LONGTEXT`（4GB）
- `TINYBLOB`、`MEDIUMBLOB`、`LONGBLOB`

エンジンはどれも `TEXT` / `BLOB` として持つことになる。
違いは**宣言された型名と、それが生む結果列メタデータ・`SHOW CREATE TABLE` の出力**だけ。

## 難所

この 4 箇所が食い違うと、静かに壊れる。

1. **CREATE TABLE の受け付け**（MySQL → SQLite 方向）:
   `mysql/parser/lib.rs` の `render_column()` と `declared_character_length()`
2. **`SHOW CREATE TABLE` の出力**（Turso AST → MySQL 方向）:
   `mysql/parser/mysql_ddl.rs` の `render_mysql_type()`
3. **結果列のワイヤ型**:
   `mysql/server/src/frontend_adapter.rs` の `mysql_type_for_declared_name()`
4. **`SHOW COLUMNS` / `information_schema.COLUMNS` の型名表示**:
   `mysql/server/src/frontend_adapter/catalog_results.rs` の `show_column_type_name()`

**過去に踏んだ罠:** `mysql_type_for_name()`（エンジンが推論した型名から引く）と
`mysql_type_for_declared_name()`（宣言された型名から引く）は別物。
文字列リテラルの推論型も "TEXT" になるため、前者を変えると
リテラルの結果列型まで変わってしまう。**宣言側だけを変えること。**

## 先に計測すること

```sql
CREATE TABLE t (
  id INT NOT NULL PRIMARY KEY,
  a TINYTEXT, b TEXT, c MEDIUMTEXT, d LONGTEXT,
  e TINYBLOB, f BLOB, g MEDIUMBLOB, h LONGBLOB
);
SHOW CREATE TABLE t;
SHOW COLUMNS FROM t;
SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, CHARACTER_OCTET_LENGTH
  FROM information_schema.COLUMNS WHERE TABLE_NAME = 't' ORDER BY ORDINAL_POSITION;
INSERT INTO t (id, a, b, c, d) VALUES (1, 'x', 'x', 'x', 'x');
SELECT a, b, c, d, e, f, g, h FROM t;
```

`--column-type-info` で最後の `SELECT` の結果列を記録する。**8 列すべて。**

- 各列の `column_type`（252 BLOB になるはず。TEXT 系も BLOB 型で返る）
- **`column_length`。** サイズごとに違う値になる。実測値をそのまま表に写す。
- `character_set`（TEXT 系は utf8mb4、BLOB 系は binary の 63 のはず）
- `BLOB` フラグ(16)、`BINARY` フラグ(128)、`NOT_NULL`
- `SHOW CREATE TABLE` が返す型名の綴り（小文字か大文字か）

**この計測結果の表がタスクの成果物の中心。** テストにも同じ表を書く。

## 実装

1. `mysql/parser/lib.rs` の `render_column()`
   - 型名を認識して SQLite 側の型に落とす。TEXT 系は `TEXT`、BLOB 系は `BLOB`。
   - `sqlparser` がこれらをどの `DataType` variant にするかを実際に確認する
     （`DataType::TinyText` などがあるはず）。
2. `mysql/parser/mysql_ddl.rs` の `render_mysql_type()`
   - Turso の AST は `TEXT` / `BLOB` としか持っていない可能性が高い。
     **その場合、`SHOW CREATE TABLE` は `TINYTEXT` を `text` と答えてしまう。**
     ここが最大の設計判断。
     - 案 A: 宣言された型名を何らかの形で保持する（大きい変更）
     - 案 B: 保持せず、`SHOW CREATE TABLE` が元の綴りを失うことを
       **COMPAT.md に明記した既知の差異として受け入れる**
   - 案 B を採るなら、それを承知のうえで結果列メタデータだけ正しくする
     ことになるが、**結果列メタデータも宣言型名に依存している**ので、
     宣言型名が失われると `column_length` も正しく出せない。
     **着手前にここを調べ、案 A が必要なら、その調査結果を持って
     このタスクを 2 つに割ること。**
3. `mysql/server/src/frontend_adapter.rs` の `mysql_type_for_declared_name()`
   - 6 つの型名を足す。`mysql_type_for_name()` は**触らない**。
4. `mysql/server/src/frontend_adapter/catalog_results.rs` の `show_column_type_name()`
   - `SHOW COLUMNS` の `Type` 列に返す綴りを足す。

## テスト

- パーサ: 6 つの型で `CREATE TABLE` が通り、SQLite 側の型に落ちること。
- エンドツーエンド: 8 列のテーブルを作り、
  **`SELECT` の結果列 8 つすべての型・長さ・character_set・フラグ**が
  計測した表と一致すること。`SHOW CREATE TABLE` と `SHOW COLUMNS` の出力も。

## ドキュメント

- `mysql/TODO.md` の「Column types」から
  「`TINYTEXT` / `MEDIUMTEXT` / `LONGTEXT`, and the `BLOB` sizes」の行を消す。
- `SHOW CREATE TABLE` で綴りが失われるなら `mysql/COMPAT.md` の
  「Known divergences」に相当する箇所に必ず書く。
