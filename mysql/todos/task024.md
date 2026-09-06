# task024: `SAVEPOINT` / `ROLLBACK TO SAVEPOINT` / `RELEASE SAVEPOINT`

**難易度: 中** / 参照: `../TODO.md` 「Transactions and locking」

まず [README.md](README.md) の共通ルールを読むこと。

## これは何か

トランザクション内の中間地点を作り、そこまで巻き戻す。
`BEGIN` / `COMMIT` / `ROLLBACK` と `SET autocommit` はすでに通っている。

ORM がネストしたトランザクションを表現するのにほぼ必ず使うので、
実用上の価値は高い。

## 計測済みの挙動

MySQL 8.4.11 で確認済み:

```sql
START TRANSACTION;
INSERT INTO sp VALUES (1);
SAVEPOINT s1;
INSERT INTO sp VALUES (2);
ROLLBACK TO SAVEPOINT s1;
SELECT id FROM sp;      -- 1 のみ
RELEASE SAVEPOINT s1;
COMMIT;
SELECT id FROM sp;      -- 1 のみ
```

`ROLLBACK TO SAVEPOINT` は**トランザクションを終わらせない**。
セーブポイント以降だけを巻き戻し、その後も同じトランザクションが続く。
ここが `ROLLBACK` との決定的な違い。

## 最初に確かめること

**エンジンが `SAVEPOINT` を持っているか。**
SQLite は持っているが、Turso が実装しているとは限らない。
**着手して最初にこれを確認すること。** 無ければこのタスクは
「エンジンに SAVEPOINT を実装する」という別の大きな作業に化ける。
その場合は調査結果を `../TODO.md` に書いて、ここでは止めること。

```bash
cargo run -q --bin tursodb -- -q ":memory:" <<'SQL'
CREATE TABLE sp (id INTEGER PRIMARY KEY);
BEGIN;
INSERT INTO sp VALUES (1);
SAVEPOINT s1;
INSERT INTO sp VALUES (2);
ROLLBACK TO s1;
SELECT id FROM sp;
RELEASE s1;
COMMIT;
SELECT id FROM sp;
SQL
```

MVCC 経路と WAL 経路の両方で確かめること
（`docs/agent-guides/mvcc.md` と `docs/agent-guides/transaction-correctness.md`）。
片方だけ動く場合は、動かないほうを拒否する。

## 触る箇所

1. **パース**: `mysql/parser/lib.rs` の `parse_optional_transaction_command()` と
   `MySqlTransactionCommand`、および `mysql/parser/admin_command.rs` の
   `transaction_token_kind()`。`BEGIN` / `COMMIT` / `ROLLBACK` を読んでいる
   すぐ隣に足す。
   - `SAVEPOINT <identifier>`
   - `ROLLBACK TO [SAVEPOINT] <identifier>`（`SAVEPOINT` は省略可）
   - `RELEASE SAVEPOINT <identifier>`
   - **識別子の検査**: 引用符付き、予約語、長すぎるものをどう扱うか。
     `consume_admin_table_name()` が既存の識別子読み取りの手本。
     **エンジンに渡す前に必ず検証すること**（SQL インジェクションの入口になる）。
2. **実行**: `mysql/frontend/session.rs` の `execute_transaction_command()`。
   `is_transaction_command()` も対応する必要がある。
3. **トランザクション状態**: `ROLLBACK TO` の後もトランザクションは続くので、
   `SERVER_STATUS_IN_TRANS` フラグを**落としてはいけない**。
   `connection_status_flags()`（`mysql/server/src/frontend_adapter.rs`）を確認する。
   ここを間違えると、クライアントが「トランザクションは終わった」と誤解する。
4. **autocommit との関係**: `SET autocommit = 0` の暗黙トランザクション中にも
   `SAVEPOINT` が使える。既存の `begin_implicit_transaction_for_write()` との
   噛み合わせを確認する。

## 追加で計測すべきこと

```sql
SAVEPOINT s1;                              -- トランザクション外（エラーか無視か）
START TRANSACTION;
ROLLBACK TO SAVEPOINT nosuch;              -- 存在しない（1305 のはず）
SAVEPOINT s1; SAVEPOINT s1;                -- 同名の再定義
SAVEPOINT s1; SAVEPOINT s2; ROLLBACK TO s1; RELEASE SAVEPOINT s2;  -- 巻き戻し後の s2
COMMIT;
START TRANSACTION; SAVEPOINT s1; COMMIT;   -- COMMIT でセーブポイントが消えるか
ROLLBACK TO SAVEPOINT s1;
```

エラー番号とメッセージを記録し、
`mysql/server/src/response.rs` の対応表に足すべきものがあるか判断する。
無ければ既存の `FrontendErrorKind` に寄せる。

## テスト

- パーサ: 3 つの文と `ROLLBACK TO`（`SAVEPOINT` 省略形）がパースされること。
  識別子が不正なものが拒否されること。
  **`ROLLBACK` 単体が従来どおり動き続けること**——
  `ROLLBACK TO` と取り違えると全トランザクションが壊れるので、
  ここは明示的にテストする。
- エンドツーエンド: 上の計測どおりの行が残ること。
  **`ROLLBACK TO` の後もトランザクションが続いていること**
  （続けて `INSERT` して `COMMIT` し、両方の行が残ることで確認する）。
  存在しないセーブポイントへの巻き戻しが正しいエラーになること。

## ドキュメント

- `mysql/TODO.md` の「Transactions and locking」から
  `SAVEPOINT` の行を消す。
- `mysql/COMPAT.md` のトランザクションの節に書く。
  MVCC と WAL で挙動が違うなら必ず明記する。
