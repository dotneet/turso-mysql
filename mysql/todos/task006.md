# task006: `GREATEST` / `LEAST` を通す

**難易度: 中** / 参照: `../TODO.md` 「Functions → Not looked at → Numbers」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`GREATEST(a, b, c)` / `LEAST(a, b, c)` — 引数のうち最大／最小を返す。可変長引数。

エンジンには複数引数のスカラ `max` / `min` がある（`core/function.rs`。
集約の `max` / `min` とは別物）。名前が集約と衝突するので、翻訳先が本当に
スカラ版として解釈されるかを `tursodb` で必ず確かめること。

**中難易度である理由:** 結果の型が引数の型の組み合わせで決まる。
MySQL には型の昇格規則があり、整数と文字列を混ぜると挙動が変わる。
最初のスライスでは**全引数が同じ種類（すべて整数列／リテラル、またはすべてテキスト）**に
限って受け、混在は拒否するのが安全。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, a INT, b INT, s VARCHAR(20), u VARCHAR(5));
INSERT INTO t VALUES (1, 3, 7, 'apple', 'kiwi');

SELECT GREATEST(a, b) FROM t;
SELECT LEAST(a, b) FROM t;
SELECT GREATEST(a, b, 100) FROM t;
SELECT GREATEST(s, u) FROM t;        -- テキスト同士（照合順序が効くか）
SELECT GREATEST(s, 'Banana') FROM t; -- 大文字小文字
SELECT GREATEST(a, NULL) FROM t;     -- NULL が 1 つでもあれば NULL か
SELECT GREATEST(a, s) FROM t;        -- 型混在（これは拒否する方針だが挙動は記録する）
```

`--column-type-info` で記録する。

- 整数引数のときの `column_type` と `column_length`
- **テキスト引数のときの `column_length` が、引数のうち最も広いものになるか**。これが核心。
- `NOT_NULL` フラグ（引数に NULL 可能列があれば落ちるはず）
- 型混在時の結果型（実装では拒否するが、記録しておくと後続タスクが楽になる）

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT max(3, 7, 100), min(3, 7), max('apple','Banana'), max(3, NULL);
SQL
```

**`max(3, NULL)` が NULL になるか 3 になるかは engine と MySQL で食い違いやすい。必ず両方で測る。**
食い違うなら、`ifnull` などで包んで揃えるか、NULL 可能列を拒否する。

## 実装

- `mysql/parser/static_select_metadata.rs` の `scalar_call()`:
  `CONCAT` の分岐が最も近い（可変個の引数を走査して `columns` と `literal_characters` を積む）。
  - `named(&["GREATEST", "LEAST"])`。引数は 2 個以上。
  - 全引数が「列」または「同じ種類のリテラル」であること。混在は `None` を返して拒否。
  - 新しい `ScalarFunction` variant（例: `Widest`）を足し、`columns` に列名を全部入れる。
- `mysql/parser/translate.rs` の `render_scalar_call()`:
  `GREATEST` → `max(...)`、`LEAST` → `min(...)`。引数は `render_scalar_arguments(function)?`。
- `mysql/server/src/frontend_adapter.rs` の `scalar_call_column_definition()`:
  `Concatenates` の分岐が手本。全列のメタデータを引いて、
  **幅は合計ではなく最大**を取る。`is_text_column` の判定が全列で揃っていなければ
  `FrontendErrorKind::Unsupported` を返す。

## テスト

- パーサ: 整数列・テキスト列それぞれの翻訳後 SQL。型混在、引数 1 個、
  リテラルと列の混ざり方が想定外のものの拒否。
- エンドツーエンド: 値（NULL を含む場合を必ず入れる）と、
  **結果列の `column_length` が最も広い引数の幅になっていること**。

## ドキュメント

- `mysql/TODO.md` の「Numbers」行から `GREATEST`、`LEAST` を消す。
  型混在を落とすならその 1 行を残す。
- NULL の扱いや照合順序で食い違うなら `mysql/COMPAT.md` に書く。
