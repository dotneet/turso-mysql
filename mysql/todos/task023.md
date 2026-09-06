# task023: ユーザー変数 `SET @x = 1` / `SELECT @x`

**難易度: 中** / 参照: `../TODO.md` 「Session variables」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`SET @x = 1` で接続ごとの変数に値を入れ、`SELECT @x` で読む。
`@@name`（システム変数）とは別物。**接続に紐づく状態**なので、
セッションが持つ必要がある。

多くのクライアント、ORM、マイグレーションツールが使う。
`SET @old_mode = @@sql_mode` のような形は `mysqldump` の出力にも現れる。

## 計測済みの結果メタデータ

MySQL 8.4.11 で計測済み。**型が代入された値で変わり、しかも一貫していない。**

| 状態 | ワイヤ型 | `column_length` | `decimals` | Collation | フラグ |
|---|---|---|---|---|---|
| `SET @x = 1` の後の `SELECT @x` | LONGLONG (8) | 21 | 0 | binary (63) | `BINARY`(128) `NUM`(32768) |
| `SET @s = 'abc'` の後の `SELECT @s` | MEDIUM_BLOB (250) | 16777215 | 31 | **latin1_swedish_ci (8)** | **なし** |
| 未定義の `SELECT @undefined` | VAR_STRING (253) | 65535 | 31 | binary (63) | `BINARY` |

**文字列の場合だけ collation が latin1_swedish_ci になり、フラグが 1 つも立たない。**
utf8mb4 ではない。ここは推測では絶対に当たらないので、上の表をそのまま使うこと。

`SET @y := @x + 1` も動き（`:=` 形）、結果は LONGLONG 21 で `@x` と同じ形。
未定義変数の読み出しは**エラーではなく NULL**。

## 触る箇所

1. **パース（代入側）**: `mysql/parser/session_settings.rs` に
   `parse_optional_session_setting()` があり、`SET` 文の入口になっている。
   隣に `SET @name = <literal>` を読む経路を足す。
   `=` と `:=` の両方を受けること。
   `mysql/parser/admin_command.rs` の手書きトークナイザが
   `@` をどのトークンにするかを確認する。**足りないなら足す。**
2. **パース（読み出し側）**: `mysql/parser/session_queries.rs` に
   `parse_optional_system_variable_query()`（`@@name` 用）がある。
   その隣に `SELECT @name` を読む経路を足す。
   **`@@` と `@` を取り違えないこと。** 既存のシステム変数の経路を壊さないよう、
   既存テストが通り続けることを必ず確認する。
3. **保持**: 接続ごとの `HashMap<String, 値>`。
   `mysql/frontend/session.rs` の `MySqlConnection`、または
   `mysql/server/src/connection_state.rs` のどちらが適切かを、
   **既存のセッション状態（`sql_mode`、`time_zone`、`autocommit`）が
   どちらに置かれているかを読んでから**決める。同じ場所に置くこと。
   - 変数名は大文字小文字を区別するか。**計測すること**
     （`SET @x = 1; SELECT @X;`）。
   - `COM_RESET_CONNECTION` で消えるか。**計測すること。**
4. **結果セット**: `mysql/server/src/frontend_adapter.rs`。
   保持している値の種類（整数か文字列か未定義か）で上の表のとおりに
   列を組み立てる。`column_definition()` と `set_column_flags()` を使う。
   **文字列の場合に `character_set` を latin1_swedish_ci の 8 にすること**を忘れない
   （既存コードは utf8mb4 を既定にしている）。

## スコープの切り方

第 1 段は**リテラルの代入と、そのままの読み出し**だけでよい。

- `SET @x = 1`、`SET @s = 'abc'`、`SET @n = NULL`
- `SELECT @x`（射影が変数 1 個だけ）

落としてよいもの（落としたら `../TODO.md` に残す）:

- `SET @y := @x + 1` のような式（第 1 段では拒否）
- `SET @x = (SELECT ...)`
- `SELECT @x, id FROM t` のような列との混在
- `SELECT @x := id FROM t`（射影内での代入。MySQL の特殊機能）

## 追加で計測すべきこと

```sql
SET @x = 1;
SELECT @X;                       -- 大文字小文字
SET @n = NULL; SELECT @n;        -- NULL の型とフラグ
SET @f = 1.5; SELECT @f;         -- 小数
SET @b = TRUE; SELECT @b;
SELECT @x, @s;                   -- 複数
SET @x = 1, @s = 'a';            -- 1 文で複数代入
SELECT @x + 1;
```

## テスト

- パーサ: `SET @x = 1` と `SELECT @x` がパースされること。
  **`SET @@sql_mode = ...` や `SELECT @@version` が従来どおり動き続けること**
  （既存のシステム変数テストが通ること）。落とす形が拒否されること。
- エンドツーエンド: 同一接続で `SET` → `SELECT` が往復すること。
  **結果列の型・長さ・decimals・collation・フラグが上の表と一致すること**
  （整数・文字列・未定義の 3 通り）。
  **別接続では見えないこと。**

## ドキュメント

- `mysql/TODO.md` の「Session variables」から
  「User variables」の行を消す。落とした形は 1 行残す。
- `mysql/COMPAT.md` のセッション変数の節に、計測した表を書く。
  文字列の collation が latin1_swedish_ci である点は特に明記する。
