# task018: `SHOW TABLES LIKE 'pattern'` を通す

**難易度: 中** / 参照: `../TODO.md` 「Statements and administration」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`SHOW TABLES LIKE 'foo%'` — 名前でテーブルを絞る形。
多くのクライアントとマイグレーションツールが「このテーブルはあるか」を
これで尋ねる。`SHOW TABLES` 自体はすでに通っている。

`../TODO.md` には `SHOW TABLES` / `SHOW COLUMNS` の `LIKE` と `WHERE` が
1 行にまとめて載っているが、**このタスクは `SHOW TABLES LIKE` と
`SHOW FULL TABLES LIKE` だけ**を扱う。`WHERE` 句と `SHOW COLUMNS LIKE` は別タスク。

## 有利な材料

`mysql/parser/like_pattern.rs` に `MySqlLikePattern` がすでにある。
何のために作られたか、どこから使われているかを最初に読むこと
（`SHOW FULL TABLES` や `information_schema` の経路で使われている可能性がある）。
パターンの意味づけはそこにまとまっているはずなので、
**新しくパターン照合を書かないこと。**

## 先に計測すること

```sql
CREATE TABLE alpha (id INT NOT NULL PRIMARY KEY);
CREATE TABLE beta (id INT NOT NULL PRIMARY KEY);
CREATE TABLE Alpaca (id INT NOT NULL PRIMARY KEY);

SHOW TABLES;
SHOW TABLES LIKE 'a%';        -- 大文字小文字（Alpaca が出るか）
SHOW TABLES LIKE 'A%';
SHOW TABLES LIKE '%a';
SHOW TABLES LIKE 'alph_';     -- _ が 1 文字
SHOW TABLES LIKE 'nomatch';   -- 0 行のときの結果列
SHOW FULL TABLES LIKE 'a%';
SHOW TABLES LIKE 'a\\%b';     -- エスケープ
```

`--column-type-info` で記録する。

- **結果列の名前が `LIKE` の有無で変わるか。** MySQL は `Tables_in_<db>` を返すが、
  `LIKE` を付けると `Tables_in_<db> (a%)` のように**パターンが列名に入る**ことがある。
  ここは必ず実測すること。既存の `show_tables_column(database)` が
  列名を組み立てているので、そこを直すことになる。
- 大文字小文字の扱い（`LIKE 'a%'` が `Alpaca` を拾うか）
- 0 行のときも列は返るか

## 実装

1. **パース。** `mysql/parser/lib.rs` の `parse_optional_show_tables()` と
   `mysql/parser/show_full_tables.rs` の `parse_optional_show_full_tables()`。
   どちらも手書きトークナイザ（`mysql/parser/admin_command.rs`）を使っている。
   - `TABLES` の後に `LIKE` と文字列リテラルが続く形を読む。
     文字列リテラルの読み取りは `mysql/parser/admin_command.rs` の
     `consume_admin_string_literal()` がある。**private なので `pub(crate)` に上げ、
     `lib.rs` の `use admin_command::{...}` に足すこと**（`show_full_tables.rs` の
     ように `use super::{...}` で引く形でもよい）。
   - `MySqlShowCommand` / `MySqlShowFullTablesCommand` にパターンを持たせる。
   - **`LIKE` の後に何も続かない、リテラルでない、末尾にゴミがある場合は
     従来どおり拒否する。**
2. **絞り込み。** テーブル一覧を作っているのは
   `mysql/frontend/session/catalog.rs` の `list_tables()`、
   結果セットにしているのは
   `mysql/server/src/frontend_adapter/catalog_results.rs` の
   `show_tables_result_to_execution_result()` と
   `show_full_tables_result_to_execution_result()`。
   - **絞り込みをどちらでやるかを決める。** 一覧をすべて取ってから
     結果セット側で絞るのが差分は小さいが、`list_tables()` には
     `TABLE_LIST_SCAN_LIMIT` による打ち切りがある。
     打ち切られた一覧をパターンで絞ると、**「打ち切られた」という事実が
     見えなくなる。** どちらにするか判断し、理由をコメントに書くこと。
   - 照合には `MySqlLikePattern` を使う。大文字小文字の扱いを計測に合わせる。
3. **列名。** 計測で列名にパターンが入るなら、
   `show_tables_column(database)` にパターンを渡せるようにする。

## テスト

- パーサ: `SHOW TABLES LIKE 'a%'` と `SHOW FULL TABLES LIKE 'a%'` が
  パターン付きでパースされること。`LIKE` の後が空、リテラルでない、
  末尾ゴミありが拒否されること。
- エンドツーエンド: 3 つのテーブルを作り、
  **大文字小文字違いの名前を必ず含めて**絞り込み結果が MySQL と一致すること。
  0 行のときも結果列が返ること。**結果列の名前**が計測値と一致すること。

## ドキュメント

- `mysql/TODO.md` の「`SHOW TABLES` / `SHOW COLUMNS` with `LIKE` or `WHERE`」を、
  残った形（`WHERE` 句、`SHOW COLUMNS LIKE`）の行に書き直す。
- `mysql/COMPAT.md` の `SHOW TABLES` の節に 1 文足す。
  一覧の打ち切りと絞り込みの順序について判断した内容も書く。
