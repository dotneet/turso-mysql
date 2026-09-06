# task008: `HEX` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Not looked at → Strings」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`HEX(str)` — 引数の各バイトを 16 進 2 桁で表した文字列を返す。
数値を渡した場合は数値の 16 進表現になるので、**文字列引数と数値引数で意味が変わる**。

エンジンにも `hex` がある（`core/function.rs`）。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), n INT);
INSERT INTO t VALUES (1, 'abc', 255);

SELECT HEX(name) FROM t;
SELECT HEX(n) FROM t;          -- 数値引数（'FF' になるか '323535' になるか）
SELECT HEX('あ');               -- マルチバイト
SELECT HEX(NULL);
SELECT HEX('');
```

`--column-type-info` で記録する。

- 結果の `column_type`（VAR_STRING のはず）と `character_set`
  （**binary の 63 になるか utf8mb4 になるか**。ここは要確認）
- **`column_length` が元の列幅の何倍か。** バイトを 2 桁で表すので、
  `元のバイト幅 × 2` が素直だが、utf8mb4 の 1 文字 4 バイトを見込んで
  `文字数 × 4 × 2` になっている可能性がある。実測値をそのまま採る。
- `NOT_NULL` フラグ

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT hex('abc'), hex(255), hex('あ'), hex('');
SQL
```

**数値引数での食い違いに注意。** MySQL の `HEX(255)` は `'FF'`、
SQLite 系の `hex(255)` は数値を文字列にしてからバイト列にするため `'323535'` になることがある。
食い違うなら、**数値列に対する `HEX` は拒否**して文字列列だけ受けるのが安全。
拒否したなら `../TODO.md` にその 1 行を残す。

## 実装

README.md の「スカラ関数を 1 つ足すときの型」に従う。

- `mysql/parser/static_select_metadata.rs` の `scalar_call()`:
  末尾の「1 引数・引数は列」の分岐群に `named(&["HEX"])` を足す。
  新しい `ScalarFunction` variant（例: `Hexadecimal`）を足す。
- `mysql/parser/translate.rs` の `render_scalar_call()`: `hex(...)`。
- `mysql/server/src/frontend_adapter.rs` の `scalar_call_column_definition()`:
  `is_text_column(source)` でなければ `Unsupported`（数値引数を拒否する方針の場合）。
  幅は計測した倍率で組み立てる。`saturating_mul` を使う。

## テスト

- パーサ: 翻訳後 SQL。数値列に対する `HEX` を拒否するならその確認。
- エンドツーエンド: 値と結果列の `column_type` / `column_length` / `character_set` / フラグ。

## ドキュメント

- `mysql/TODO.md` の「Strings」行から `HEX` を消す。数値引数を落とすならその 1 行を残す。
- 食い違いが残るなら `mysql/COMPAT.md` に書く。
