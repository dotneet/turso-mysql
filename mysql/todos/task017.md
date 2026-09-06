# task017: `SHOW COUNT(*) WARNINGS` / `SHOW COUNT(*) ERRORS` を通す

**難易度: 低** / 参照: `../TODO.md` 「Statements and administration」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

直前の文が出した警告／エラーの**件数だけ**を返す形。
`SHOW WARNINGS` / `SHOW ERRORS` はすでに通っている。

`SHOW COUNT(*) WARNINGS` は `SELECT @@warning_count` と同じ値を返す。
`SHOW COUNT(*) ERRORS` は `SELECT @@error_count` と同じ。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY);
DROP TABLE IF EXISTS nosuch;   -- 警告を 1 件出す
SHOW COUNT(*) WARNINGS;
SHOW WARNINGS;
SHOW COUNT(*) WARNINGS;        -- SHOW WARNINGS 自体は件数を消さないか
SHOW COUNT(*) ERRORS;
SELECT 1;
SHOW COUNT(*) WARNINGS;        -- 普通の文の後で 0 に戻るか
```

`--column-type-info` で結果列を記録する。**ここが実装の中身のほとんど。**

- 列名（`@@session.warning_count` のような形になるはず。実測した文字列をそのまま使う）
- `column_type`（8 LONGLONG のはず）
- `column_length`、`decimals`
- フラグ: `NOT_NULL`(1)、`BINARY`(128)、`UNSIGNED`(32)、`NUM`(32768) が立つか

`SHOW COUNT(*) WARNINGS` を実行した後に件数がどうなるかも記録する
（`SHOW WARNINGS` と同じく、件数を消さないはず）。

## 実装

1. **パース。** `mysql/parser/lib.rs` の `parse_optional_show_warnings()` と
   `parse_optional_show_diagnostics()` が既存の入口。手書きトークナイザ
   （`mysql/parser/admin_command.rs`）を使っている。
   - `SHOW`、`COUNT`、`(`、`*`、`)`、`WARNINGS` の並びを読む。
     `tokenize_admin_command()` が `(` `*` `)` をどのトークンにするかを
     `admin_command.rs` の `AdminToken` で確認すること。**足りないトークン種別が
     あるならトークナイザ側に足す必要がある。** そこが唯一の非自明な点。
   - `admin_command.rs` の関数は既定で private。クレートルートから呼ぶものは
     `pub(crate)` にし、`lib.rs` の `use admin_command::{...}` に足す。
   - `MySqlShowWarningsCommand` に「件数だけ」を表すフィールドを足すか、
     別のコマンド型を作る。既存の `MySqlShowWarningsCommand` が
     `LIMIT` を持っているはずなので、そこに寄せるのが素直。
2. **結果セット。** `mysql/server/src/frontend_adapter.rs` の
   `show_warnings_result()` / `show_errors_result()` の隣に、
   1 列 1 行の結果を組み立てる関数を書く。
   `column_definition(name, MYSQL_TYPE_LONGLONG)` と `set_column_flags(...)` で
   計測したフラグを立てる。
3. **ディスパッチ。** `execute_query` からこの新しいコマンドへ分岐する箇所を、
   既存の `SHOW WARNINGS` の分岐の隣に足す。

## テスト

- パーサ（`mysql/parser/tests.rs`）: `SHOW COUNT(*) WARNINGS` と
  `SHOW COUNT(*) ERRORS` がパースされること。
  `SHOW COUNT(1) WARNINGS`、`SHOW COUNT (*) WARNINGS` の空白違い、
  末尾ゴミ付きなどの扱いを決めてテストに書く。
- エンドツーエンド（`mysql/server/src/frontend_adapter/tests.rs`):
  **警告を実際に 1 件出してから**件数が 1 になること、
  普通の文の後で 0 に戻ること、`SHOW WARNINGS` を挟んでも件数が消えないこと。
  結果列の名前・型・長さ・フラグが計測値と一致すること。

## ドキュメント

- `mysql/TODO.md` の「Statements and administration」から
  「`SHOW COUNT(*) WARNINGS`, `SHOW COUNT(*) ERRORS`」の行を消す。
