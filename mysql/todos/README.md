# 実装タスク指示書

`../TODO.md` のうち、比較的難易度の低いものを 1 タスク 1 ファイルに切り出したもの。
各ファイルは単独で読めるように書いてあるが、**全タスク共通のルールはこのファイルにだけ書く**ので、
着手前に必ずここを読むこと。

タスクが完了したら、その `taskNNN.md` を削除し、`../TODO.md` の該当行を更新する。

---

## 共通ルール

### 絶対にやってはいけないこと

- **未コミットの差分を捨てない。** `git stash` / `git reset --hard` / `git checkout -- .` は禁止。
  作業開始時はまず `git status`、`git diff --check`、`git diff --cached`、必要ならファイル単位の `git diff` を見る。
- **`mysql/server/.tmp*` と `rust_out` には触らない。**
- **ワークスペース全体の `cargo fmt` は禁止。** 整形の乱れは、機能差分を壊さない狭いパッチで直す。
  新規ファイルだけは、内容が確定した後に `rustfmt --edition 2021 <その1ファイル>` をかけてよい。
- **`--release` でビルドしない。**
- **バックグラウンドプロセスや CPU 負荷を残さない。** サブエージェントを使う場合も同じことを伝える。
- **Docker は専用コンテナを一意な名前で作り、使い終わったら消す。** 既存コンテナを名前で使い回さない。

### MySQL の挙動は推測しない

差異は必ず、ダイジェスト固定したオラクルで**計測**してから実装する。

```bash
NAME=turso-mysql-oracle-$(date +%s)
docker run -d --name $NAME -e MYSQL_ROOT_PASSWORD=oracle -e MYSQL_DATABASE=probe \
  mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb
# 起動待ち
until docker exec $NAME mysqladmin -uroot -poracle ping 2>/dev/null | grep -q alive; do sleep 2; done
# 計測
docker exec -i $NAME mysql -uroot -poracle probe -t <<'SQL'
SELECT VERSION();
SQL
# 後始末（必ず実行する）
docker rm -f $NAME
```

結果メタデータ（型・長さ・フラグ）まで見たいときは、`mysql` クライアントの `--column-type-info` を使う。

```bash
docker exec -i $NAME mysql -uroot -poracle probe --column-type-info -t <<'SQL'
SELECT LOWER(name) FROM t;
SQL
```

エンジン（Turso 側）の挙動は次で確かめる。

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
SELECT lower('AbC');
SQL
```

### 主要なファイル

| 何をするところ | ファイル |
|---|---|
| MySQL SQL を SQLite テキストに翻訳する本体 | `mysql/parser/translate.rs` |
| 結果列の「形」を後段に渡すメタデータ | `mysql/parser/static_select_metadata.rs` |
| クレートの公開型・エラー・文の入口 | `mysql/parser/lib.rs` |
| 管理系文の手書きトークナイザ | `mysql/parser/admin_command.rs` |
| `information_schema` クエリの検査 | `mysql/parser/information_schema.rs` |
| Turso AST → MySQL DDL の描画 | `mysql/parser/mysql_ddl.rs` |
| 接続とスキーマ読み取り | `mysql/frontend/session.rs`、`mysql/frontend/session/catalog.rs` |
| ワイヤ上の結果列を決める最終地点 | `mysql/server/src/frontend_adapter.rs` |
| カタログ系の結果セット組み立て | `mysql/server/src/frontend_adapter/catalog_results.rs` |
| エラーコードの対応表 | `mysql/server/src/response.rs` |

テストはそれぞれ `mysql/parser/tests.rs`、`mysql/frontend/session/tests.rs`、
`mysql/server/src/frontend_adapter/tests.rs` にある。

### スカラ関数を 1 つ足すときの型

関数追加系のタスクはすべてこの 4 箇所を触る。既存の `LOWER` / `CONCAT` / `LEFT` を読めば形が分かる。

1. `mysql/parser/static_select_metadata.rs` の `scalar_call()`
   — 名前を認識し `StaticSelectMetadata::ScalarCall { function, columns, literal_characters, not_null }` を返す。
   既存の `ScalarFunction` の種別で表せないなら、新しい variant を足す。
2. `mysql/parser/translate.rs` の `render_scalar_call()`
   — エンジンが実際に走らせる SQL に落とす。エンジン関数名が MySQL と違う／意味が違う場合はここで吸収する。
3. `mysql/server/src/frontend_adapter.rs` の `scalar_call_column_definition()`
   — 計測した型・長さ・フラグを組み立てる。新しい `ScalarFunction` variant を足したならここに分岐が要る。
4. テストとドキュメント（下記）。

**罠:** `sqlparser` は `TRIM` / `FLOOR` / `CEIL` / `SUBSTRING` などに専用の AST ノードを与えるため、
`Expr::Function` として来ない関数がある。実装前に、対象の関数が `Expr::Function` で来るかを必ず確かめること。

### エンジンが持っている関数名（`core/function.rs` より）

MySQL 名とエンジン名が食い違うものが多い。実装前に `core/function.rs` で必ず確認すること。
既知の食い違いの例:

- MySQL `LENGTH`（バイト数） → エンジン `octet_length`
- MySQL `CHAR_LENGTH`（文字数） → エンジン `length`
- MySQL `REVERSE` → エンジン `string_reverse`
- MySQL `POW` → エンジン `pow` / `power`
- MySQL `TRUNCATE` → エンジン `trunc`
- MySQL `CONCAT` は NULL 引数があれば全体が NULL、エンジンの `concat()` は NULL を飛ばす（`||` を使う）

### テストの方針

**変更なしで落ち、変更ありで通るテストを必ず書く。** 最低 2 段:

- パーサ単体: `mysql/parser/tests.rs`。`parse_select(...)` の `as_sql()` が期待どおりの SQLite テキストになること。
- エンドツーエンド: `mysql/server/src/frontend_adapter/tests.rs`。実際に `CREATE TABLE` → `INSERT` →
  対象クエリを流し、**行の値**と（該当するなら）**結果列の型・長さ・フラグ**を確認する。

テストのコメントには「MySQL 8.4.11 で計測した」事実を書く。推測を書かない。

### ドキュメント

- 動くようになったものは `mysql/TODO.md` の該当行を消す。
- MySQL と食い違う点が残るなら `mysql/COMPAT.md` の該当節に散文で書く。TODO.md には差異を書かない。

### ゲート（コミット前に必ず通す）

```bash
cargo test -p turso_mysql -p turso_mysql_server -p turso_mysql_parser -p turso_mysql_runtime
cargo clippy -p turso_mysql -p turso_mysql_server -p turso_mysql_parser -p turso_mysql_runtime \
  --all-features --all-targets -- --deny=warnings
```

`git add` した後に `git diff --cached` で index を必ず確認してからコミットする。

### コミットメッセージ

```text
mysql: <小文字の命令形の要約>

<なぜ必要か、どの不変条件やバグに関わるか>

<計測した MySQL の挙動、非自明な実装上の判断>

Tests: <実行した検証>

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```

差分をなぞらず意図を書く。コード中のコメントと COMPAT.md の散文は**英語**で、周囲の文体に合わせる。

---

## タスク一覧

| # | 内容 | 難易度 |
|---|---|---|
| [001](task001.md) | `REPLACE(str, from, to)` | 低 |
| [002](task002.md) | `REVERSE` と `REPEAT` | 低 |
| [003](task003.md) | `LPAD` / `RPAD` | 低 |
| [004](task004.md) | `LOCATE` / `INSTR` | 低 |
| [005](task005.md) | `MOD` / `POW` / `SQRT` / `SIGN` | 低 |
| [006](task006.md) | `GREATEST` / `LEAST` | 中 |
| [007](task007.md) | `NULLIF` | 低 |
| [008](task008.md) | `HEX` | 低 |
| [009](task009.md) | `COUNT(DISTINCT col)` | 低 |
| [010](task010.md) | `GROUP_CONCAT` | 中 |
| [011](task011.md) | `<=>`（NULL 安全等価） | 低 |
| [012](task012.md) | 修飾された列との比較 `WHERE t.id = 1` | 中 |
| [013](task013.md) | `UPDATE` / `DELETE` の `WHERE ... IN (...)` | 低 |
| [014](task014.md) | ワイルドカード射影に対する `ORDER BY <序数>` | 中 |
| [015](task015.md) | `CROSS JOIN` | 中 |
| [016](task016.md) | 括弧付き `UNION` 枝 | 低 |
| [017](task017.md) | `SHOW COUNT(*) WARNINGS` / `ERRORS` | 低 |
| [018](task018.md) | `SHOW TABLES LIKE 'pattern'` | 中 |
| [019](task019.md) | `TINYTEXT` / `MEDIUMTEXT` / `LONGTEXT` と `BLOB` 各サイズ | 中 |
| [020](task020.md) | `ORDER BY` / `LIMIT` 付きの `UPDATE` / `DELETE` | 中 |
| [021](task021.md) | `UNSIGNED` 整数（結果メタデータは計測済み） | 中〜高 |
