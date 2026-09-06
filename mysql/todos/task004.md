# task004: `LOCATE` / `INSTR` を通す

**難易度: 低** / 参照: `../TODO.md` 「Functions → Not looked at → Strings」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

部分文字列の位置を 1 始まりで返す。見つからなければ 0。
**引数の順序が逆なので注意。**

- `LOCATE(substr, str)` — 探すものが先
- `INSTR(str, substr)` — 探される側が先
- `LOCATE(substr, str, pos)` — 3 引数版もある

エンジンには `instr(str, substr)` がある（`core/function.rs`）。
つまり `INSTR` は引数そのまま、`LOCATE` は**引数を入れ替えて**落とす必要がある。
ここを間違えると、対称な入力では気付けない静かなバグになる。テストで必ず非対称な入力を使うこと。

## 先に計測すること

```sql
CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20));
INSERT INTO t VALUES (1, 'abcabc');

SELECT LOCATE('b', name) FROM t;
SELECT INSTR(name, 'b') FROM t;
SELECT LOCATE('B', name) FROM t;    -- 大文字小文字（照合順序が効くか）
SELECT LOCATE('z', name) FROM t;    -- 見つからない
SELECT LOCATE('', name) FROM t;     -- 空文字
SELECT LOCATE('b', name, 3) FROM t; -- 3 引数版
SELECT LOCATE('b', NULL);
SELECT LOCATE('あ', 'あいう');        -- マルチバイト（文字単位かバイト単位か）
```

`--column-type-info` で記録する。

- `column_type`（整数系のはず。8 LONGLONG か 3 LONG か）
- `column_length`、`decimals`、`NOT_NULL` と `NUM`（32768）フラグ
- **大文字小文字を無視するか。** MySQL の既定照合順序では `LOCATE('B','abc')` が 2 を返す
  可能性がある。エンジンの `instr` はおそらく区別する。ここが食い違うなら、
  COMPAT.md に書くか、`lower()` を挟んで揃えるかを判断する。

エンジン側:

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT instr('abcabc','b'), instr('abcabc','B'), instr('abcabc','z'), instr('あいう','あ');
SQL
```

## 実装

- `mysql/parser/static_select_metadata.rs` の `scalar_call()`:
  `LOCATE` / `INSTR` の分岐。引数 2 つのみ受ける（3 引数版は最初のスライスでは拒否してよい。
  拒否するなら `../TODO.md` にその旨を残す）。
  結果は整数なので、既存の `ScalarFunction` に合うものがなければ新 variant
  （例: `ScalarFunction::CountsText` と同じ扱いでよいか計測して判断）を使う。
  `CountsText`（`LENGTH` / `CHAR_LENGTH`）は「テキスト列を受けて整数を返す」ので、
  型・長さ・フラグが計測値と一致するならそのまま流用できる。**一致するか必ず確かめる。**
- `mysql/parser/translate.rs` の `render_scalar_call()`:
  - `INSTR` → `instr(<arg0>, <arg1>)`
  - `LOCATE` → `instr(<arg1>, <arg0>)` ← **入れ替える**
- `mysql/server/src/frontend_adapter.rs`: 流用しないなら分岐を足す。

## テスト

- パーサ: **`LOCATE('b', name)` と `INSTR(name, 'b')` が同じ SQL に落ちること**を明示的に確認する。
  これが引数入れ替えの回帰テストになる。3 引数版が拒否されること。
- エンドツーエンド: 見つかる場合・見つからない場合の値と、結果列の型・長さ・フラグ。

## ドキュメント

- `mysql/TODO.md` の「Strings」行から `LOCATE`、`INSTR` を消す。
  3 引数版を落とすなら、その 1 行を残す。
- 大文字小文字の扱いが食い違うなら `mysql/COMPAT.md` に書く。
