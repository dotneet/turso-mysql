# 停止時に保存した候補・証拠

本ディレクトリは再開用の保全資料であり、実装済み・統合済みという意味ではありません。
最新の状態・検証不足・再開順は `../mysql-handoff-2026-09-05.md` を参照してください。
停止後に新しい実装やテストは行わず、既存の差分と証拠だけをコピーしています。
`SHA256SUMS` はこのディレクトリで `shasum -a 256 -c SHA256SUMS` により確認できます。

## パッチの取り扱い

| ファイル | 基準・用途 | 注意 |
|---|---|---|
| `root-wip.patch` | HEAD `823df6fb8` に対する停止時のtracked3ファイル差分 | 既にrootに存在。再適用禁止 |
| `schemata.patch` | parser78622a…/adapter4140f9…を基準 | 既にrootへ適用済み。root-wipと重複 |
| `typed-not-null.patch` | isolated b9c91de58基準、10ファイル | rootのconflict safety-netを含む。重複・古いadapter置換に注意 |
| `ordinary-missing-default.patch` | b9系typed候補上、frontend2ファイル | 全frontend testsとレビュー未完。equality sessionを保持して移植 |
| `not-null-wire.patch` | b9系runtimeへの72行追加 | compile-only。ordinary omission修正なしでは1364期待が失敗する |
| `not-null-gate-selector.patch` | 旧gate scriptをコピーした提案 | selector追加案だけ。新script名/ベースを確認して移植 |
| `information-schema-tables.patch` | 370系adapter基準、1ファイル | full server/独立レビュー未完。source-metadata/SCHEMATAを保持 |
| `strict-integer-comparisons.patch` | equality parser78622a…/session4e4a0c…基準 | root SCHEMATAとparserが重なる。独立レビュー/実測未完 |
| `parameter-ordinal.patch` | HEAD823df、Core statement/tests | getterだけ。marker本体ではない。レビュー/full Core未完 |
| `show-create-unreviewed.patch` | HEAD823dfと候補4ファイルの差分 | 大量の整形ノイズあり。そのまま適用禁止。full server失敗/Oracle未実測 |

全patchの構文は停止時に `git apply --stat` で読み取れることを確認しました。
これは現在rootへの適用可否・テスト成功の確認ではありません。
実際の移植前には、各patchのbaseと対象ファイル内容を再確認してください。
保存形式のdiff contextの空白を整形しないでください。

元の `information-schema-tables.patch` は末尾に余分な空行があり、保存版ではそれだけ省略されています。
元SHAは `d5bb0451a70d15097c78625149cd88fe689d60f0898580471214db7cb661829d`、
保存版SHAは `f87e9313e127df63e701c612c011febb9dc2a82ffd356fcdaa3897bfd09e976b`。
実装hunkの内容は同一です。他の提供済み主要パッチは元SHAと一致します。

## 実測証拠

- `linux-selectors.txt` / `linux-exits.txt` / `linux-source-hashes.txt` /
  `linux-artifact-hashes.txt`: kc4QPCの限定7/7 gate。現在のSCHEMATAや他候補を含まない。
- `prepared-marker-provenance.txt`: 明示的なfresh owned MySQL fixture、終了・cleanup記録。
- `prepared-marker-observations.jsonl`: NULL/integer/real/text/coercion/charset/connection-reset。
- `prepared-reset-observations.jsonl`: C APIによるCOM_STMT_RESET。
- `prepared-reprepare-observations.jsonl`: schema変更後の自動reprepare。
- `prepared-marker-design.md`: 観測と実装候補を分けた設計メモ。未実測の遷移を確定扱いしない。

raw build/test logsやさらに古いOracle結果のパスは本ハンドオフに記録しています。
大量のビルドキャッシュ、container、credential、未知の`.tmp*`や`rust_out`は保存対象にしていません。
