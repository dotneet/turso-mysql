# task022: `DATE` / `TIME` / `YEAR` 型と日付関数

**難易度: 高（分割推奨）** / 参照: `../TODO.md` 「Column types」「Functions → Waiting on a column type」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`DATETIME` と `TIMESTAMP` は通っているが、`DATE` / `TIME` / `YEAR` が無い。
そしてこの 3 型が無いことが `CURDATE()`、`CURRENT_DATE`、`DATE_ADD`、
`DATE_SUB`、`DATEDIFF`、`YEAR()`、`MONTH()`、`DAY()` を全部止めている。
`../TODO.md` が「blocks `CURDATE()` and the date functions」と書いているのはこれ。

**分割して進めること。** 推奨する順序:

1. `DATE` 型だけ（`CURDATE()` / `CURRENT_DATE` が動くようになる）
2. `TIME` 型（`CURTIME()`）
3. `YEAR` 型（フラグが特殊。下記）
4. `YEAR()` / `MONTH()` / `DAY()` の抽出関数
5. `DATE_ADD` / `DATE_SUB` / `DATEDIFF`

## 計測済みの結果メタデータ

MySQL 8.4.11 で計測済み。`Collation` はいずれも binary (63)。

**列として:**

| 宣言 | ワイヤ型 | `column_length` | `decimals` | フラグ |
|---|---|---|---|---|
| `DATE` | DATE (10) | 10 | 0 | `BINARY`(128) |
| `TIME` | TIME (11) | 10 | 0 | `BINARY` |
| `YEAR` | YEAR (13) | 4 | 0 | `UNSIGNED`(32) `ZEROFILL`(64) `NUM`(32768) |
| `DATETIME` | DATETIME (12) | 19 | 0 | `BINARY` |
| `TIMESTAMP` | TIMESTAMP (7) | 19 | 0 | `BINARY` |

**`YEAR` 列だけ `ZEROFILL` が立ち、`BINARY` が立たない。** ここは見落としやすい。

**関数の結果として:**

| 式 | ワイヤ型 | `column_length` | フラグ |
|---|---|---|---|
| `CURDATE()` / `CURRENT_DATE` | DATE | 10 | `NOT_NULL` `BINARY` |
| `CURTIME()` | TIME | 8 | `NOT_NULL` `BINARY` |
| `YEAR(NOW())` | YEAR | 4 | `UNSIGNED` `BINARY` `NUM` |
| `MONTH(NOW())` | LONGLONG (8) | 3 | `BINARY` `NUM` |
| `DAY(NOW())` | LONGLONG | 3 | `BINARY` `NUM` |

**`CURTIME()` は 8、`TIME` 列は 10。** 同じ型でも幅が違う。
**`YEAR(NOW())` は `ZEROFILL` が立たない**（列と違う）。
**`MONTH` / `DAY` は YEAR ではなく LONGLONG で幅 3。**

## 触る箇所

1. **型の受け付け**: `mysql/parser/lib.rs` の `render_column()`。
   SQLite 側には日付型が無いので `TEXT` に落ちる（`DATETIME` が
   今どう落ちているかを読んで、同じ扱いにする）。
2. **`SHOW CREATE TABLE`**: `mysql/parser/mysql_ddl.rs` の `render_mysql_type()`。
3. **結果列**: `mysql/server/src/frontend_adapter.rs` の
   `mysql_type_for_declared_name()` と `mysql_table_column_flags()`。
   `MYSQL_TYPE_DATE` / `TIME` / `YEAR` の定数と `ZEROFILL` フラグを足す必要がある。
4. **バイナリプロトコル**: `BinaryRowValue` / `BinaryRowColumnType`
   （`mysql/server/src/` のプリペアド経路）。
   **ここを忘れると、テキストでは動くのにプリペアドで `Internal` エラーになる。**
   過去に CHAR / DECIMAL / DATETIME / TIMESTAMP でまさにこれが起きている。
   **プリペアド `SELECT` のテストを必ず書くこと。**
5. **値の検証**: `mysql/frontend/session.rs`。`DATETIME` が不正な値を
   どう弾いているか（`IncorrectTemporalValue`）を読み、同じ扱いを `DATE` / `TIME` に広げる。
6. **関数**: README.md の「スカラ関数を 1 つ足すときの型」に従う。
   `NOW()` が `ScalarFunction::Now` として既にあるので、それが直接の手本。
   `render_scalar_call()` では `date('now')` / `time('now')` などに落ちる
   （エンジンに何があるかを `core/function.rs` と `tursodb` で確認すること）。

## 追加で計測すべきこと

```sql
CREATE TABLE d (id INT NOT NULL PRIMARY KEY, a DATE, b TIME, c YEAR);
INSERT INTO d VALUES (1, '2026-09-07', '12:34:56', 2026);
INSERT INTO d VALUES (2, '2026-02-30', '12:34:56', 2026);  -- 不正な日付
INSERT INTO d VALUES (3, '2026-09-07', '99:99:99', 2026);
INSERT INTO d VALUES (4, '2026-09-07', '12:34:56', 1899);  -- YEAR の範囲外
SHOW WARNINGS;
SELECT a FROM d WHERE a = '2026-09-07';
SELECT a FROM d WHERE a > '2026-01-01';                    -- 比較の型規則
SELECT b FROM d;                                           -- TIME の負値や 24 時間超
SHOW CREATE TABLE d;
SELECT DATE_ADD('2026-09-07', INTERVAL 1 DAY);
SELECT DATEDIFF('2026-09-08', '2026-09-07');
```

**`INTERVAL` は `sqlparser` が専用の AST ノードにする**ので、
`DATE_ADD` は `Expr::Function` として来ない可能性が高い。
`TRIM` / `FLOOR` / `SUBSTRING` と同じ罠。着手前に確認すること。

比較の型規則（`checked_comparison_fits_column()` が
整数かテキストかしか見ていない）に日付をどう足すかは設計判断。
**第 1 段では日付列に対する `WHERE` 比較を拒否**しても、
`SELECT` して表示するだけで十分に価値がある。

## テスト

- パーサ: 3 型の `CREATE TABLE`、`SHOW CREATE TABLE` の往復。
- エンドツーエンド: 列の型・長さ・フラグが上の表と一致すること
  （**`YEAR` の `ZEROFILL` を必ず確認**）。
  **テキストプロトコルとプリペアド（バイナリ）プロトコルの両方**で
  値が読み出せること。不正な値の `INSERT` が MySQL と同じエラーになること。

## ドキュメント

- `mysql/TODO.md` の「Column types」と
  「Functions → Waiting on a column type」から通したものを消す。
- `mysql/COMPAT.md` の型の節に計測した表を書く。
  `TIMESTAMP` の既知の差異（ゾーン変換しない）と同種の差異があれば併記する。
