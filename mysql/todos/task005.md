# task005: `MOD` / `POW` / `SQRT` / `SIGN` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Not looked at → Numbers」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

数値スカラ関数 4 つ。どれも `mysql/parser/static_select_metadata.rs` の
`scalar_call()` の末尾にある「1 引数・引数は列」の分岐群と、`ABS`（`KeepsNumericShape`）
および `ROUND` / `CEILING`（`Truncates`）の扱いが直接の手本になる。

エンジンには `mod`、`pow`、`power`、`sqrt`、`sign` がある（`core/function.rs` で確認すること）。

**4 つを 1 タスクにまとめてあるが、1 つずつ入れてよい。** `SIGN` が最も簡単。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT, d DECIMAL(10,2), f DOUBLE);
INSERT INTO t VALUES (1, -7, 3.50, 2.25);

SELECT SIGN(n) FROM t;
SELECT SIGN(d) FROM t;
SELECT SQRT(n) FROM t;          -- 負数（NULL になるか）
SELECT SQRT(f) FROM t;
SELECT POW(n, 2) FROM t;
SELECT POW(2, 0.5);
SELECT MOD(n, 3) FROM t;        -- 負数の剰余の符号
SELECT MOD(7, 0);               -- 0 除算（NULL か、エラーか、警告か）
SELECT n % 3 FROM t;            -- 演算子形（別の AST になる。今回の対象外）
```

`--column-type-info` で結果列を記録する。特に:

- `SIGN` の型（TINY か LONGLONG か）と長さ
- `SQRT` / `POW` の型（5 DOUBLE のはず）と `decimals`
  （`NOT_FIXED_DECIMALS` = 31 になるか、固定値か）
- `MOD` の型が**引数の型で変わるか**（INT 同士なら整数、DECIMAL が絡めば DECIMAL など）。
  変わるなら、変わり方をそのまま実装に写す。整数列だけを受けて他は拒否する、という
  狭め方でもよい。狭めたなら `../TODO.md` に残す。
- `MOD(7, 0)` の結果と、その後の `SHOW WARNINGS`

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT sign(-7), sqrt(-7), sqrt(2.25), pow(-7, 2), mod(-7, 3), mod(7, 0);
SQL
```

**負数の剰余の符号と 0 除算は、言語や DB で最も食い違う箇所。必ず両方で測って突き合わせる。**

## 実装

README.md の「スカラ関数を 1 つ足すときの型」に従う。

- `SIGN` / `SQRT` は 1 引数。`scalar_call()` 末尾の分岐群に足す。
- `POW` / `MOD` は 2 引数。`LEFT` / `RIGHT` の分岐の形に近い。
  第 2 引数を数値リテラルに限るか、列も受けるかは計測結果と実装の手間で決める。
  リテラルに限るなら `../TODO.md` にその旨を残す。
- `render_scalar_call()`: それぞれ `sign(...)`、`sqrt(...)`、`pow(...)`、`mod(...)` に落とす。
  **`ROUND` / `CEILING` が `CAST(... AS INTEGER)` で包まれている理由に注意。**
  エンジンが float を返すのに MySQL が整数を返す場合、整数列が約束した型に float が来ると
  オーバーフロー扱い（1690）になる。同じ罠が `SIGN` や `MOD` にもありうるので、
  計測で MySQL が整数を返すならこちらも `CAST(... AS INTEGER)` で包む。
- `scalar_call_column_definition()`: 計測した型・長さ・`decimals`・フラグを組み立てる。
  数値なら `MYSQL_NUM_FLAG`（32768）が立つかどうかも計測して合わせる。

## テスト

- パーサ: 翻訳後 SQL。引数の個数や種類が合わないものの拒否。
- エンドツーエンド: 値（**負数を必ず含める**）と結果列の型・長さ・`decimals`・フラグ。

## ドキュメント

- `mysql/TODO.md` の「Numbers」行から通したものを消す。落とした形は 1 行残す。
- 負数の剰余や 0 除算で食い違うなら `mysql/COMPAT.md` に書く。
