# task021: `UNSIGNED` 整数を通す

**難易度: 中〜高** / 参照: `../TODO.md` 「Column types」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`INT UNSIGNED`、`BIGINT UNSIGNED` などの符号なし整数。現在はすべて拒否されている。

**実運用では極めて頻出。** 主キーを `INT UNSIGNED AUTO_INCREMENT` や
`BIGINT UNSIGNED AUTO_INCREMENT` で宣言するスキーマは珍しくないので、
これが通らないと既存スキーマの取り込みが止まる。

## 計測済みの結果メタデータ

MySQL 8.4.11 で計測済み。**この表はそのまま実装とテストに使える。**

| 宣言 | ワイヤ型 | `column_length` | フラグ |
|---|---|---|---|
| `TINYINT UNSIGNED` | TINY (1) | 3 | `UNSIGNED`(32) `NUM`(32768) |
| `SMALLINT UNSIGNED` | SHORT (2) | 5 | `UNSIGNED` `NUM` |
| `MEDIUMINT UNSIGNED` | INT24 (9) | 8 | `UNSIGNED` `NUM` |
| `INT UNSIGNED` | LONG (3) | 10 | `UNSIGNED` `NUM` |
| `BIGINT UNSIGNED` | LONGLONG (8) | 20 | `UNSIGNED` `NUM` |

`Collation` はいずれも binary (63)、`Decimals` は 0。
符号ありの `column_length`（4/6/9/11/20）より 1 桁狭いことに注意
——符号の分の桁が要らないため。`BIGINT` だけは符号あり・なしとも 20。

各型の上限値も計測済み: 255 / 65535 / 16777215 / 4294967295 /
18446744073709551615。

## 最大の難所: `BIGINT UNSIGNED` は i64 に入らない

エンジンは整数を i64 で保持する。`BIGINT UNSIGNED` の上限
18446744073709551615 は i64::MAX (9223372036854775807) の 2 倍を超えるので、
**そのままでは保持できない。**

**推奨するスライスの切り方:**

- **第 1 段: `TINYINT` / `SMALLINT` / `MEDIUMINT` / `INT` の `UNSIGNED` だけを通す。**
  上限はそれぞれ 255 / 65535 / 16777215 / 4294967295 で、すべて i64 に収まる。
  範囲検査だけで正しく扱える。
- **第 2 段以降: `BIGINT UNSIGNED` は拒否したまま**にし、
  `../TODO.md` に理由付きで 1 行残す。エンジン側に u64 を持たせるか、
  上限を i64::MAX に切り詰めて差異として記録するかは、別の判断。
  **黙って i64 に丸めてはいけない。** 主キーに使われる型なので、
  丸めは行の取り違えに直結する。

## 触る箇所

1. **`CREATE TABLE` の受け付け**（MySQL → SQLite 方向）
   `mysql/parser/lib.rs` の `render_column()`。
   `sqlparser` は `DataType::UnsignedInt(..)` のように符号なしを別 variant に
   するので、まずデバッグ出力で実際の形を確認すること。
   SQLite 側の型は `INTEGER` に落ちる。
2. **範囲検査**
   既存の符号付き整数は `MySqlNumericSpec` と `MySqlSignedInteger`
   （`mysql/parser/lib.rs`）で下限・上限を持っている。
   符号なし側の下限は 0、上限は上の表。
   `stored_decimal_size()` の近くにある数値仕様の組み立てを読むこと。
   **`INSERT` で範囲外の値が来たときの MySQL の挙動を計測すること**
   （strict モードでは 1264 Out of range value のはず）。
3. **`SHOW CREATE TABLE` の出力**（Turso AST → MySQL 方向）
   `mysql/parser/mysql_ddl.rs` の `render_mysql_type()`。
   Turso の AST が「符号なし」という事実を保持できるかが鍵。
   保持できないなら task019（TEXT/BLOB のサイズ）と同じ設計問題に当たる。
   **着手して最初に確認すること。** 保持できないなら、
   結果列の `UNSIGNED` フラグも立てられないので、タスク全体の前提が変わる。
4. **結果列のワイヤ型とフラグ**
   `mysql/server/src/frontend_adapter.rs` の `mysql_type_for_declared_name()` と
   `mysql_table_column_flags()`。`MYSQL_UNSIGNED_FLAG`(32) は定義済み。
   `mysql_type_for_name()`（エンジン推論の型名から引くほう）は**触らない**。
5. **`SHOW COLUMNS` / `information_schema.COLUMNS` の型名**
   `mysql/server/src/frontend_adapter/catalog_results.rs` の
   `show_column_type_name()`。`int unsigned` のような綴りを計測して合わせる。
6. **`AUTO_INCREMENT` との組み合わせ**
   `mysql/parser/lib.rs` の `CheckedAutoIncrementCreateTable` 周辺と
   `mysql/frontend/session.rs` の採番。
   **`INT UNSIGNED AUTO_INCREMENT` が実運用の主目的**なので、
   ここまで通して初めて価値が出る。採番の上限が符号なしの上限になることを確認する。

## 追加で計測すべきこと

```sql
CREATE TABLE u (id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, n INT UNSIGNED);
SHOW CREATE TABLE u;
SHOW COLUMNS FROM u;
SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, NUMERIC_PRECISION
  FROM information_schema.COLUMNS WHERE TABLE_NAME = 'u' ORDER BY ORDINAL_POSITION;

INSERT INTO u (n) VALUES (4294967295);
INSERT INTO u (n) VALUES (4294967296);   -- 範囲外（1264 のはず）
INSERT INTO u (n) VALUES (-1);           -- 負数（1264 のはず）
SHOW WARNINGS;

SELECT n FROM u WHERE n = 4294967295;
SELECT n + 1 FROM u;                     -- 演算結果の型と長さ
SELECT SUM(n) FROM u;                    -- 集約の結果型
```

`SHOW CREATE TABLE` が返す綴り（`int unsigned` か `int(10) unsigned` か）と、
`n + 1` や `SUM(n)` の結果列メタデータを記録すること。
算術と集約は `mysql/parser/static_select_metadata.rs` の
`ArithmeticShape` / `ColumnAggregateKind` が担当していて、
符号なしを想定していない。**第 1 段では符号なし列に対する算術と集約を
拒否してよい**（拒否するなら `../TODO.md` に残す）。

## テスト

- パーサ: 4 つの型で `CREATE TABLE` が通ること。`BIGINT UNSIGNED` が
  拒否されること。範囲外リテラルの拒否。
- エンドツーエンド: 4 列のテーブルを作り、
  **結果列の型・長さ・`UNSIGNED` フラグ**が上の表と一致すること。
  上限値の往復。範囲外の `INSERT` が MySQL と同じエラーになること。
  `INT UNSIGNED AUTO_INCREMENT` で採番できること。

## ドキュメント

- `mysql/TODO.md` の「Column types」の `UNSIGNED` 行を、
  残った形（`BIGINT UNSIGNED`、符号なし列の算術・集約）に書き直す。
- `mysql/COMPAT.md` の整数型の節に、計測した表と `BIGINT UNSIGNED` を
  拒否する理由（i64 に入らない）を書く。
