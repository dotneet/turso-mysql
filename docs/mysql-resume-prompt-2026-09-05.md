# 次エージェントへ渡す再開プロンプト

以下をそのまま渡してください。

```text
/Users/shinji/projects/turso-mysql の MySQL 互換モード実装を再開してください。
前エージェントはユーザー指示で停止済みです。全体目標は未完です。

停止文書作成後に「現状でcommit/push」の指示があり、停止時の実装3ファイルと引き継ぎ資料は
チェックポイントコミットへ保存されています。下記の823df/未コミットという説明は停止時の履歴です。
現在の正確なHEADはgit logで確認し、保存済み差分を重複適用しないでください。
チェックポイント化のために追加テストや実装はしていません。未完ゲートは未完のままです。

まず必ず以下を全文読んでください。
- AGENTS.md
- docs/mysql-handoff-2026-09-05.md（最新。旧ハンドオフより優先）
- docs/mysql-handoff-2026-09-05-artifacts/README.md
- docs/mysql-handoff-2026-09-04.md（歴史的背景。古い未完記述に注意）
- .claude/skills/code-quality/SKILL.md
- .claude/skills/testing/SKILL.md
Core IO/transaction変更には対応する追加skillも使ってください。

最初の操作は git status、git diff --check、git diff --cached、各ファイルのdiff確認です。
停止時 HEAD/origin/main は 823df6fb8a704f2ea6b7594b00c95deedc04f189。
18b6e4090（crypto）、bab6b913a（result source metadata）、823df6fb8（signed integer equality）
は検証・独立レビュー後にcommit/push済みです。

未コミット実装は SCHEMATA の parser/adapter 2ファイルと
tests/integration/conflict_resolution.rs の NOT NULL safety-net です。
SCHEMATA は候補のfocused/full/strict・独立レビューが済み、root適用後の統合ゲートは未実行です。
停止時に開始しようとしていたcargo metadataも中断され、test実行は始まっていません。
まずこの統合ゲートを完了し、2ファイルだけのcommit/pushを検討してください。

未コミット差分を絶対に破棄・stash・reset・一括restoreしないでください。
mysql/server/.tmp* と rust_out は所有不明なので触らないでください。
workspace-wide cargo fmtは禁止。既存整形ノイズは機能差分を保った狭いpatchで扱います。
docsのartifactにある root-wip.patch / schemata.patch は既にrootにある変更で、再適用禁止です。
他の候補patchもbaseを確認し、現在ファイル全体を古いsnapshotで置き換えないでください。

親はオーケストレーション・調整・統合レビュー・テスト・commit/pushに注力してください。
通常サブエージェントは Luna / xhigh。難しい場合だけ Sol / high または Astra / medium。
なるべく独立した仕事で並列枠を使い、rootの同じファイルを複数担当で編集しないこと。
専用コピーは必ず新しい一意の場所に作り、base commit/overlay SHA/ownerを記録します。
/tmp/turso-mysql-sql-wip.8KY0uD は過去に変更され、もうfrozen sourceではありません。
既存コピーやDocker containerを名前や推測で使わず、所有と作成履歴を確認してください。
新規disposable MySQL fixtureは担当を一人に限定し、ID/DB/endpoint/cleanupを記録します。

次の並列担当候補:
A. typed NOT NULL 1048 + ordinary INSERT/prepared省略列の1364 +実driver E2E。
   typed本体だけでは新wire testの省略列1364が失敗します。DEFAULT VALUESへテストを弱めず、
   ordinary-missing-default候補の全test/独立レビューを完了してください。
B. SCHEMATA統合後のTABLES metadata、SHOW CREATE TABLE。
   SHOW CREATEは整形ノイズ、full server失敗、Oracle未実測が残り、そのまま適用不可です。
C. signed integer大小比較。専用候補MRQuYxで全parser/frontendとstrict済み、レビュー/実測未完。
D. prepared marker。parameter ordinal getter候補あり。本体のeffective type/charset/coercionは未実装。
   正式MySQL測定と設計メモはartifactに保存されています。

各スライスはfocused test、影響crateの全test、strict --all-features --all-targets clippy、
独立レビュー後に機能別commit/pushしてください。staged indexも必ず確認してください。
テストはsource freeze、raw logs、command/tee exit、source/binary SHAを記録します。
Linux privileged testsのcompile-only/ignoredは成功に数えません。
zshの予約変数statusを使わず、失敗は直ちに報告します。

既存Linux7/7はSCHEMATA等を含まない以前の限定snapshot証拠です。
25-step equality比較はSQL全部成功ですがprepared markerの10metadata差分でprofile FAILのままです。
P7 trigger rollback差はpre-existing別件で、1048の小さい変更に混ぜません。
driver/ORM、broader privileges/TLS/catalog、P7の全体目標を勝手に縮めないでください。
保守性・作業効率・優先順位を定期的に振り返り、根拠に基づいて改善してください。

停止時の定期振り返りautomation mysqlはPAUSEDです。重複作成せず、再開の必要性は別途判断してください。
まず最新状態の確認結果と所有分担を短く報告してから作業してください。
```
