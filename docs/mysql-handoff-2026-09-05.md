# MySQL 互換モード停止時ハンドオフ — 2026-09-05

## 停止後のチェックポイント保存について

この文書の作成後、ユーザーから「いったん現状でcommit/pushして」と明示指示があった。
そのため、以下に「未コミット」と記した実装3ファイルと、本書・再開プロンプト・保存artifactを
まとめて停止時チェックポイントとしてコミットする。これは実装再開や品質ゲート完了の宣言ではない。
SCHEMATAのroot統合ゲートは未実行のまま。追加のテストは行っていない。
正確なチェックポイントhashは `git log -1` で確認すること。
以下の HEAD823df/未コミット/index空という記述は、文書作成時点の履歴として読むこと。
再開時はこのチェックポイントを保持し、root-wip/schemata patchを重複適用しない。
所有不明の `.tmp*` と `rust_out` はコミット対象外。

## 最初に読むこと

ユーザーの明示的な停止依頼により、実装・テストを停止した。
全体目標「MySQL 互換モードを完成させる」は未達成であり、完了扱いにしない。
別エージェントはユーザーから再開を指示された後に作業すること。
現在状態については本書を `docs/mysql-handoff-2026-09-04.md` より優先する。
旧ハンドオフには、既に完了したゲートやコミットを未完と記す古い箇所がある。

- Repository: `/Users/shinji/projects/turso-mysql`
- Branch: `main`
- 停止時 `HEAD == origin/main == 823df6fb8a704f2ea6b7594b00c95deedc04f189`
- 最終 push 成功を確認済み。停止後は実装コミット・push をしていない。
- 全サブエージェントは停止し、引き継ぎ報告だけを返して終了済み。
- 停止後の process check で当タスク所有のビルド・テストプロセスは見つからなかった。
  最終確認時に残っていた cargo は cwd で `scala-rs` 所有と確認し、別タスクなので触っていない。
- 定期振り返り automation `mysql` は `PAUSED` に更新成功。
  自動再開しない。全体 goal は未完のまま（完了/blocked に偽装していない）。
- 本書・再開プロンプト・保存 artifact は停止依頼への文書化として追加した未コミットファイル。

## 作業上の必須ルール

1. `AGENTS.md`、本書、旧ハンドオフ、`.claude/skills/code-quality/SKILL.md`、
   `.claude/skills/testing/SKILL.md` を読んでから、status / diff --check / 各ファイル diff を確認する。
   Core IO に触るなら async-io-model、トランザクションなら transaction-correctness も読む。
2. 未コミット差分は絶対に stash / reset / 一括 restore / discard しない。
   `mysql/server/.tmp*` と `rust_out` は所有不明。削除・変更しない。
3. workspace-wide `cargo fmt` 禁止。既存行の整形差分を混ぜない。必要なら狭い apply_patch。
   release ビルド禁止。変更ごとに focused test、影響 crate 全 test、strict clippy、独立レビュー。
4. 通常のサブエージェントは **Luna / xhigh**。Sol / high、Astra / medium は難しい場合だけ。
   既存 Astra タスクを通常業務で再利用しない。親は所有調整・統合・レビュー・ゲート・commit/push。
5. 並列作業先は必ず新しい一意の専用コピーを作り、base commit、overlay SHA、owner を記録。
   既存 `/tmp` コピーを「空いていそう」という理由で編集しない。
   同じ parser/session/adapter ファイルは root で並列編集しない。
6. Docker/DB fixture は作成担当を一人に限定。名前だけで既存 container を選ばない。
   新規 disposable fixture の ID/DB/endpoint/作成ログ/cleanup を記録。
   未確認 container の環境変数・credential を取得しない。secret をログ・文書・コミットに出さない。
7. 大きなゲートは source を freeze し、command exit と tee exit、raw log、source/binary hash を保存。
   zsh の予約変数 `status` を使わない。Bash PIPESTATUS または task 固有の変数を使う。
   compile-only / ignored / 中断を実 E2E 成功と呼ばない。
8. 本書のパッチは自動適用用キューではない。base/対象/source hash を確認して機能単位で統合する。
   staged index も確認し、context のあるパッチを使う。過去に zero-context staging で誤適用があった。

## 今回までに確定・push 済みの主なコミット

| Commit | 内容 |
|---|---|
| `823df6fb8` | strict signed integer SELECT equality、parameter ordinal、reprepare 検査、Unix driver regression |
| `bab6b913a` | Core source-column provenance、text/prepared の table/alias/original name/key/default flags |
| `18b6e4090` | 暗号化 opt-in/key 検証、接続初期化 reservation、Pager 所有 ATTACH lease、URI page-one 初期化 |
| `370f5a205` | preopened tests の global DATABASE_MANAGER clear 7件を除去（並列テスト干渉防止） |
| `adfe439a3` | 以前の equality 比較証拠とゲート状況の文書化 |
| `e60286d8b` | 25-step equality reference case/default-six golden/限定 comparator profile |
| `b2c62b0a9` | 実測された warning だけを3 goldenへ反映 |
| `b9c91de58` | result terminator 後の warning/status/affected rows 等の観測修正 |
| `9144a33d7` | ordinary signed INT/INTEGER primary key |
| `662e183cb` | information_schema.COLUMNS の任意 validated target（selected DB 内） |
| `0cdb705cd` | prepared schema reprepare metadata |

それ以前の SMALLINT/MEDIUMINT/BIGINT、COM_RESET_CONNECTION + Pool、quota、TCP/TLS、
table privileges、SHOW TABLES/COLUMNS、DDL/SQL notes 等は旧ハンドオフと履歴を参照。
最初の再開依頼にあった5ファイルだけが現在の WIP ではない。

## root の未コミット実装（停止時に3ファイル）

| ファイル | 所有機能 | SHA256 |
|---|---|---|
| `mysql/parser/lib.rs` | SCHEMATA parser | `20ba17e56764a59b27fd6d5aaf2025c6bf66703a934a91aa1bcc0eb535883584` |
| `mysql/server/src/frontend_adapter.rs` | SCHEMATA provider/tests/import 配置 | `e3a71bfb3794439f52b9b00d41b062bbe67cee7c239281d804e91d6dad708ae0` |
| `tests/integration/conflict_resolution.rs` | NOT NULL safety-net tests。typed production は含まない | `419b2b46ea38bbc526d4504bb458c7174b484c8be1c216775222d9cd05096786` |

停止直前の index は空。`git diff --check` は成功。
追加した文書/artifact 以外の unknown untracked `.tmp*`/`rust_out` はすべて保全した。
実装3ファイルの完全な差分は本書隣の artifact directory の `root-wip.patch` にも保存。
**これは既に root に存在する変更であり、再適用しない。**

### SCHEMATA の直前状態

`SELECT SCHEMA_NAME FROM information_schema.SCHEMATA` の狭い1投影のみ。
List 権限、selected DB 不要、row/value/payload/memory bounds、unsupported shape の fail-closed。
parser/provider/test の独立レビュー APPROVE（information_schema 全体の parity ではない）。

- 元候補: `/private/tmp/turso-mysql-schemata-wip.mXzacd`
- 元 base: `24c832ada` + SQL overlay（parser `78622a...`、adapter `4140f9...`）。
- 単独候補 parser 85、focused server 3、full server 572、strict parser/server all-features/all-targets が成功。
- 最終 full server は trusted source copy
  `/Users/shinji/projects/turso-mysql/target/.codex-schemata-source-20260905` で実施。
  `/private/tmp` manifest/cwd の earlier 437/135、553/19 は InvalidRoot 等の失敗ログとして保持。
  これらを成功として数えない。
- 統合前に root 由来の新しいコピーで patch 適用後 hash を再確認し、期待値2件と完全一致。
  `/private/tmp/turso-mysql-schemata-patch-verify-exact.VjuMbr`
- 親が root に適用済み。**root 統合後 parser/server full test と strict clippy は未実行**。
  担当は cargo metadata/process inventory の開始直後に停止され、テストは一つも始めていない。
- 最初の再開作業は、この2ファイルを freeze して統合ゲートを完了すること。

## 確定済み変更の検証証拠

### Linux 実 driver gate

`/private/tmp/turso-mysql-final-gate-harness.UTOCGW/evidence.kc4QPC/`

- snapshot HEAD `370f5a205` + 12 tracked WIP。
- WIP patch SHA `e8e81dd2aa7eb606d0238ee87d1e60f3ab4cdb61f37cf2f7ba851b30e647a237`。
- authority 2/2、runtime 5/5 = 7/7 PASS。
  Unix bootstrap/auth/prepared/Pool reset、MEDIUMINT、quota/reset、table grants、TLS/TCP。
- build/gate command exit=0、tee exit=0、completion marker 一致、cache lock acquire/release=0。
- Linux ARM64 ELF 6 artifacts、source/current 12 SHA SAME を記録。
- raw: `raw/build-container.log`、`raw/gate.log`。selector/exit/source/artifact manifest を本書 artifact に複製。
- この後の差分は encryption test の `conn.prepare(&format!(...))` の `&` 1文字だけ。
  production はこの gate と一致したまま18b6/bab6/823dfに分割して確定した。
- **現在の SCHEMATA、分離 typed NULL、大小比較、SHOW CREATE 等はこの gate に含まれない。**
  7/7 は限定 privileged gate であり P6/P7 全体の合格ではない。

承認済み harness: `/private/tmp/turso-mysql-final-gate-harness.UTOCGW`。
cache guard SHA `7f6213bb97e53eaab318137fc8cdb06fe06acd4188bb72d5e4715adf48e7256d`。
exact cache `/private/tmp/turso-mysql-cross-uid-linux-build.MZFWuU/target` を使用。
host process / Docker mount overlap の fail-closed 検査・exclusive lock を迂回しない。
既存 cache を無条件に再利用・削除しない。

### Core/native

- Core: unit 2456 pass/17 ignored + docs 53 pass/5 ignored = **2509 pass/22 ignored**。
  `/private/tmp/turso-crypto-final-frozen-core-20260905.log`、clean exit 0。
- core_tester: unit31 + fuzz145 + integration1143 + pending-byte2 = **1321 pass/10 ignored**。
  `/private/tmp/turso-crypto-final-frozen-core-tester-20260905.log`、clean exit 0。
- strict core all-features/all-targets clippy PASS。
- core_tester strict clippy は needless borrow 1件を検出→狭い test-only 修正→rerun PASS。
  `/private/tmp/turso-crypto-final-frozen-core-tester-clippy-rerun-20260905.log`
  修正後 encryption test SHA `bbacaf95468689a97192df30f0aac1bab4860d412a0df1d196ce996ce11aa21e`。
  all-features yielded-attach focused 1件も rerun PASS。
- encryption all-features 29、VACUUM all-features 136 pass/18 ignored。
  `/tmp/turso-mysql-vacuum-gate-20260905-module.log`
- preopened 8 tests を3回並列実行 PASS。global registry clear removal は370で確定。
- equality/metadata snapshot: parser83、frontend234+2+1、server569+2、runtime11+5 ignored、strict4crates PASS。
- metadata-only 独立候補 `/Users/shinji/projects/turso-mysql-metadata-gate.3zaG0A`:
  Core metadata21、frontend232+2+1、server569+2、strict frontend/server PASS。
  index hash がこの候補と一致することを確認して bab6 を作成した。

履歴上、pre-crypto-draft では9件の production regression があった。
後の draft は Core2、VACUUM2の計4つの別の test-lifecycle/opt-in adaptation を残していた。
これらを「元から環境だけ」「最初から全部 test-only」と書き換えない。
最終 production 修正・4 test adaptation・最後の borrow 修正を経て上のゲートが通った。
古い wrapper の `status` 変数失敗や失敗ログは成功証拠に置き換えず保存している。

## MySQL 比較・prepared marker の既知差分

固定 MySQL: 8.4.11、image digest
`b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb`。
P0 は現在18ケース（17既存 + equality1）。19ではない。
equality は default-six sql_mode の実測25stepを byte-for-byte 採用。
golden SHA `231fd3b5746e647118344d4dae4c46a9c87962bd9d561f863f8c985178d31934`。
conformance 全66tests/strict clippy、pre-existing sentinel refusal/row保全も実測。

`/tmp/turso-mysql-equality-default6.SGWE78/` の実 Turso 比較:
25 SQL成功、10 metadata mismatches、0 inconclusive、profile は正しく FAIL。
すべて steps18–21 の marker `column_length` / `flags` / NULL `column_type`。
行・値・affected rows・ID・warnings/status・table columns は測定範囲で一致。
report SHA `7bdace6dd20dc69583cd49165d56b891527ddb4eb209b3dee77f48dd60331508`。
runtime SHA `3ce19bae36484f08d935e885c10ee9993cd8d6c8f1204ff38eda2dc8a5ca9abb`。

新しい正式 fresh fixture 測定:
`/private/tmp/turso-mysql-reprepare-owned.tfGkzB/`（cleanup 完了）。
主要 raw JSONL と設計メモを artifact directory に複製済み。

- initial/NULL-first: VAR_STRING、session charset、length65532、decimals31。
- integer: LONGLONG、binary charset63、length21、decimals0、BINARY flag。
- NULL は推論済み型を保持。integer後の numeric text は整数化、invalid text は0 + warning1292。
- real: DOUBLE、charset63、length23、decimals31。
- session charset45/46/255が反映され、46ではBINARY flagも付く。
- COM_STMT_RESET は推論型を保持（C APIで実測）。COM_RESET_CONNECTIONはstatementを破棄。
- ALTER TABLE後の auto-reprepare は型状態をgeneric VAR_STRINGへ戻す。
- decode用 StatementParameterType cache と projection effective type を分離する。
  wire typeだけ書き換えて値/coercion/warningを変えない実装は禁止。
- parameter projection ordinal getterだけは候補がある。effective state/charset/coercion 本体は未実装。
  real→text、blob、範囲外、new_params_bound_flag=0 の実測などは追加の契約確認が必要。

## 未統合候補一覧と次の担当分割

パッチ保存先: `docs/mysql-handoff-2026-09-05-artifacts/`。
このディレクトリの README と SHA256 manifest を必ず参照する。

### A. typed NOT NULL + ordinary omission（最優先）

`typed-not-null.patch`、元 `/private/tmp/turso-mysql-not-null-b9c91de58`（base b9c91de58）。
10ファイル、Core `NotNullConstraint { description, resolve_type }` → MySQL1048/23000。
C primary19、SDK generic Constraint等の境界、trigger FAIL の既存動作を維持。
112 conflict tests、Core/server strict clippy、独立レビュー APPROVE。
**パッチには root に既にある safety-net を含むので重複適用しない。**
root adapter は metadata/SCHEMATA が進んでいるため古いファイル全体で置換しない。

`ordinary-missing-default.patch`、元 `/private/tmp/turso-mysql-not-null-ordinary.pN0J5k`。
frontend session + insert_defaults test の2ファイルのみ。typed候補上で作成。
`Stmt::Insert.columns` を使い、通常 VALUES/prepared の省略必須列を1364/HY000へ分類。
明示 NULL は1048を維持する設計。修正前 focused failure、修正後 focused/insert_defaults3、
server focused2、frontend strict clippy PASS。**frontend 全test・独立レビュー・統合は未完**。
最後の combined command は停止指示で中断され、結果を追加計上しない。

`not-null-wire.patch` は Unix mysql_async 実test 72行、compile-only PASS、実Linux未実行。
最初の typed-only 候補では通常 INSERT省略列が1048になり、期待する1364に届かないとレビューで判明。
これを DEFAULT VALUES に弱めて通さない。ordinary omission を完成させて実driverで確認する。
selector proposal は `not-null-gate-selector.patch`。旧script名ベースなので内容を読んで統合する。
text に加え prepared explicit NULL/omission、接続継続、no partial rows も確認する。

既知 P7 trigger `INSERT OR ROLLBACK` の外側transaction/savepoint保持差は別件。
`/private/tmp/turso-mysql-p7-trigger-rollback.ABETJc/` に SQLite/無変更Core/typed候補の再現。
typed候補固有の回帰ではない。将来 `HaltIfNull` の per-instruction on_error 伝搬と
outer override precedence を検討するが、この1048 identity sliceに混ぜない。
temp-table NOT NULL UPDATE の既知失敗も `tests/fuzz/temp_tables.rs:316` 付近に残る。

### B. SCHEMATA / TABLES metadata / SHOW CREATE TABLE

SCHEMATA は既述のroot WIP。統合 full tests/strict 完了後に単独コミット。
TABLES は `information-schema-tables.patch`、元
`/private/tmp/turso-mysql-is-tables-apply.9I9aKv`。
adapterだけ63追加/10削除。origin fields、NOT_NULL/BINARY/NO_DEFAULT、
TABLE_TYPE=STRING+ENUM を既存 MySQL goldenへ合わせる。
focused5/strict PASS、**full server/独立レビュー/統合未完**。
古い370基準を含むため現在のadapterへ狭く移植し、SCHEMATAを消さない。

SHOW CREATE は `show-create-unreviewed.patch` を保存したが **そのまま適用禁止**。
元 `/tmp/turso-mysql-show-create-work` は parser/session/frontend export/server候補まであるが
rustfmt由来の大量の既存整形差分を含む。root HEAD823dfの4ファイルとのdiffを保存。
`/tmp/turso-mysql-show-create-clean` はparser/sessionだけ再移植した未完成の中間コピー。
durable DDL再構成、view/internal/missing/corrupt分類、認可/size boundsの案。
parser85/frontend236、session2/server3 focused、strict PASSという担当報告。
**full serverは最終437pass/135failのまま、Oracle未実測、独立レビュー未完**。
InvalidRoot/permissionとの担当説明を成功根拠にせず、trusted sourceで原因を確認して全testを通す。
DDLをMySQL実測と比較し、内部markerや架空のengine属性を公開しないこと。

### C. 大小比較

`strict-integer-comparisons.patch`、安全な現在の候補 `/tmp/turso-mysql-sql-owned.MRQuYx`。
parser/sessionの2ファイル。CheckedSelectComparison APIsに拡張し、< <= > >= <> !=。
i64/NULL/?、signed durable column、3VL、precedence、ordinal、reprepare、
狭い列幅より外側のRHS（tiny < 128など）を比較値として許す。
parser85、frontend236+2+1、strict all-features/all-targets PASS。
**独立レビュー、MySQL実測、root統合未完**。rootのSCHEMATA parser差分を保持して移植する。

### D. prepared marker

`parameter-ordinal.patch`、元 `/private/tmp/turso-mysql-table-ordinal.qIdhQJ`。
HEAD823dfのCore statement/test2ファイルへ70行追加。
`get_column_parameter_index` がdirect positional ?だけzero-based ordinalを返す。
named/expression/literal/EXPLAIN/boundsはNone。alias/star/?2のテスト。
focused1、metadata22、Core strict all-features/all-targets PASS。
**独立レビュー・統合未完**。full Coreはこの追加後には未実行。
これだけで marker metadata mismatch が直るわけではない。

## 作業改善・履歴上の注意

- 一人が既存 `astra-mysql-schemata-20260905-mysql-1` を名前で選び、所有者との調整なしで
  generated table を3回作成/削除した。後に当タスクのdisposable SCHEMATA project所有者は確認できたが、
  container ID/port・最初のtable snapshotは未記録。正確な同一性/影響ゼロは断定しない。
  その結果を正式fixture証拠にせず、新規 owned fixtureでreset/reprepareを取り直した。
  追加の不明資源操作はしない。ユーザーへ問題を報告済み。
- 大小比較担当が過去の equality source `/tmp/turso-mysql-sql-wip.8KY0uD` を直接編集した。
  停止・保全し、新しい MRQuYxへ分離した。旧パスはもうfreezeではない。
  旧実測は記録済み source/binary/report SHAに紐づく歴史的証拠としてのみ扱う。
  現在の root を変更した事故ではない。新しいコピーの明示所有を徹底する。
- `/private/tmp` はmode1777でaccount-storeの信頼rootチェックを失敗させる。
  test binary cwdだけ変えても埋め込みmanifest path由来の失敗が残った。
  trustedな新規source場所を使い、元の失敗ログを隠さずrerun成功を確認する。
- 失敗を検出したらコマンド一式の最後まで黙って待たず、次の境界で即報告する。
- 同じファイルでcatalog1種類を増やすたびにAST禁止節チェックが反復している。
  今回は既存の安全な形を維持し、次のcatalog追加前に狭い共通validatorを検討する。
  広いparserリファクタを検証済みfeatureと混ぜない。

## 再開順と完了条件

1. status/diff/hashを読み、本書と実態の違いを先に確認。保全済みpatchを盲目的に適用しない。
2. root SCHEMATA2ファイルの統合ゲートを完了し、機能別にcommit/push。
3. 独立担当を A=NULL identity/omission、B=catalog/SHOW CREATE、C=比較、D=marker に分ける。
   root統合は親一人。作成した専用コピー・ファイル所有表・shared target利用を明記する。
4. NULL省略列修正をレビュー/全test、typed identity と runtime test を安全に統合。
   実Linux gateに新selectorを追加し、実際の1048/1364を確認して確定。
5. TABLES/SHOW CREATE/大小比較/markerはそれぞれ候補の不足ゲートを埋めてから確定。
6. broader TCP/TLS、SQL privileges、information_schema残り、driver/ORM、P7へ順に進む。
   現状は mysql_async 0.37.1 pilot。mysql CLI/Go/Node/PyMySQL/sqlx/JDBC/ORM/mysqldump の
   完成を主張しない。P7 failure injection/recovery/stress/security/performanceも未完。
7. 保守性・優先順位・作業効率を区切りごとに再評価。既存goal/受入基準を勝手に縮めない。

旧文書更新候補は `/private/tmp/turso-mysql-docs-update-20260905/docs/` にあるが未適用。
metadata/equalityをprovisionalとする古い箇所が2件残り、SCHEMATA再検証中という記述も
停止後は未実行に直す必要がある。適用するなら本書に合わせてレビューすること。
`docs/mysql-compatibility-mode.md` の COLUMNS がrecords固定という説明は古い。
実装は662以降、selected DBの任意validated table/viewを扱う。
