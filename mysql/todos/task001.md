# task001: `REPLACE(str, from, to)` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Not looked at → Strings」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

`REPLACE('abcabc', 'b', 'XY')` のような 3 引数のスカラ関数。
`REPLACE INTO` 文とは別物なので混同しないこと（あちらは DML）。

エンジンには `replace` がある（`core/function.rs`）。名前は一致している。

## 先に計測すること

オラクル（README.md の手順）で次を測る。**結果の値だけでなく、結果列の型・長さ・フラグを
`--column-type-info` で必ず見る。**

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), note TEXT);
INSERT INTO t VALUES (1, 'abcabc', 'hello');

-- 値
SELECT REPLACE(name, 'b', 'XY') FROM t;
SELECT REPLACE(name, 'b', '') FROM t;
SELECT REPLACE(name, '', 'X') FROM t;      -- 空文字を探した場合
SELECT REPLACE(NULL, 'a', 'b');            -- NULL 引数
SELECT REPLACE(name, 'B', 'X') FROM t;     -- 探す側の大文字小文字（照合順序が効くか）

-- 結果列のメタデータ（VARCHAR(20) 列の場合、TEXT 列の場合の両方）
SELECT REPLACE(name, 'b', 'XY') FROM t;
SELECT REPLACE(note, 'l', 'LL') FROM t;
```

記録すべきこと:

- 結果の `column_type`（おそらく 253 VAR_STRING か 252 BLOB）
- `column_length` が**元の列幅のままか、置換後の最大幅まで広がるか**。これがこのタスクの核心。
  MySQL は置換で伸びうる分を見込んだ幅を返す可能性がある。実測値をそのまま採用する。
- `NOT_NULL` フラグの有無
- 大文字小文字を無視するか（`REPLACE(name,'B','X')` が置換するか）

エンジン側も確かめる:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT replace('abcabc','b','XY'), replace('abcabc','B','XY'), replace(NULL,'a','b');
SQL
```

**MySQL の `REPLACE` は照合順序に関わらず大文字小文字を区別する**と一般に言われているが、
必ず上の計測で確認すること。エンジンと食い違うなら、食い違いを COMPAT.md に書くか、
食い違う入力を拒否する。

## 実装

README.md の「スカラ関数を 1 つ足すときの型」に従う。

1. `mysql/parser/static_select_metadata.rs` の `scalar_call()`
   - `named(&["REPLACE"])` の分岐を足す。`CONCAT` の分岐が近い形（可変個の引数を見る）。
   - 引数は 3 つ。第 1 引数は `Expr::Identifier`（列）に限る。
     第 2・第 3 引数は文字列リテラルに限る（幅を静的に決めるため）。それ以外は `None` を返して拒否する。
   - 計測結果が「元の列幅のまま」なら `ScalarFunction::KeepsTextShape` が使える。
     「置換後の最大幅まで広がる」なら新しい variant（例: `ScalarFunction::Replaces`）を足し、
     `literal_characters` に幅の計算に要る文字数を入れる。
2. `mysql/parser/translate.rs` の `render_scalar_call()`
   - `replace(<arg0>, <arg1>, <arg2>)` を返す。`scalar_argument(function, n)` で引数を取れる。
3. `mysql/server/src/frontend_adapter.rs` の `scalar_call_column_definition()`
   - 新しい variant を足したならここに分岐を書く。`Concatenates` の分岐が参考になる。
   - `is_text_column(source)` でないなら `FrontendErrorKind::Unsupported` を返す
     （数値列に対する `REPLACE` の挙動は未計測なので受けない）。

## テスト

- `mysql/parser/tests.rs`: `SELECT REPLACE(name, 'b', 'XY') FROM t` が
  `SELECT replace("name", 'b', 'XY') FROM "t"` に翻訳されること。
  リテラルでない第 2・第 3 引数、引数が 3 個でないもの、第 1 引数が列でないものが拒否されること。
- `mysql/server/src/frontend_adapter/tests.rs`: 実際に流して**行の値**と
  **結果列の `column_type` / `column_length` / フラグ**が計測値と一致すること。

## ドキュメント

- `mysql/TODO.md` の「Strings」行から `REPLACE` を消す。
- 大文字小文字の扱いなどで MySQL と食い違うなら `mysql/COMPAT.md` に書く。
