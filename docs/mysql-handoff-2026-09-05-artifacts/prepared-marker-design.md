# Prepared projection marker: measured contract and implementation boundary

This is a read-only design handoff. It does not change the repository or
connect to a fixture.

## Evidence

The fresh, docs-owned MySQL 8.4.11 run is recorded under
`/private/tmp/turso-mysql-reprepare-owned.tfGkzB/`.

Observed in `expanded-stdout.jsonl` with `mysql_async` 0.37.1:

- A fresh `SELECT ? AS marker` starts as `MYSQL_TYPE_VAR_STRING`, length
  65532, decimals 31, with the negotiated character set (45 in the initial
  session).
- A first integer changes the result to `LONGLONG`, charset 63, length 21,
  decimals 0, binary flag 128. A following NULL keeps that type.
- A first real changes it to `DOUBLE`, charset 63, length 23, decimals 31,
  binary flag 128.
- After an integer has established the type, a numeric string is converted to
  the integer result. An invalid string produces zero and warning 1292. This
  was observed both for the marker-only and table-equality statements.
- Session collations 45, 46, and 255 produce the corresponding VAR_STRING
  character-set id. Collation 46 also sets binary flag 128.

Observed in `com-stmt-reset-stdout.jsonl` with libmysqlclient 9.3.0:

- `mysql_stmt_reset` preserves statement id and the inferred `LONGLONG`
  metadata. NULL after reset remains LONGLONG; real changes it to DOUBLE.
- The C API reports NUM_FLAG in addition to the binary flag; this is a C API
  metadata detail, not yet a mysql_async wire contract.

Observed in `schema-reprepare-stdout.jsonl` with mysql_async 0.37.1:

- After an automatic schema reprepare, the same statement id returns marker
  metadata to generic VAR_STRING. NULL leaves it generic; real then changes it
  to DOUBLE. The table column metadata remains intact.

## Smallest implementation boundary

1. Keep the existing `StatementParameterType` cache solely for decoding a
   `new_params_bound_flag=0` packet. Add separate per-statement effective
   projection state; do not infer the result type directly from the latest row
   value.
2. Give each prepared result column an optional parameter projection ordinal.
   Prefer a small read-only `core/statement.rs` getter for a direct variable
   expression (return `None` for literals, expressions, and out-of-range
   columns). Capture and refresh it with the existing prepared result-column
   metadata. This avoids SQL/name matching and keeps aliases/star ordinals
   positional.
3. For a column with that ordinal, start/reset the effective state as generic,
   ignore NULL for state changes, and apply only measured transitions initially:
   generic→integer, generic→real, generic→text, integer→numeric text, and
   NULL retaining an established numeric type. Convert the row value together
   with the chosen metadata. Keep static, declared, source-table, literal,
   and information_schema metadata authoritative and untouched.
4. Put the narrow integer-string conversion result and warning count in the
   result-building path. Warning 1292 and the zero value must be generated
   together; do not only relabel a text row as LONGLONG. Range, decimal,
   blob/binary, and other direction transitions need separate oracle evidence
   before implementation.
5. Carry the negotiated handshake character-set id through
   `CommandExecutionOptions` into generic VAR_STRING column definitions.
   Default test builders to 45. Numeric/blob results keep charset 63. Do not
   overwrite durable information_schema/source metadata; the SCHEMATA oracle
   remains charset 45.
6. Preserve effective state over `COM_STMT_RESET`, clear it over
   `COM_RESET_CONNECTION`, and reset it only when automatic schema reprepare
   is actually detected. The last rule needs an explicit refresh/reprepare
   boundary; clearing on every ordinary statement reset would contradict the
   measured `COM_STMT_RESET` behavior.

## Ownership and validation

- `core/statement.rs`: parameter-projection ordinal getter and narrow boundary
  tests (bounds, direct variable, star/alias ordering).
- `mysql/frontend/session.rs`: carry/capture the ordinal in
  `MySqlPreparedResultColumnTypeMetadata` and expose the refresh/reprepare
  boundary; coordinate with the current source-reference owner.
- `mysql/server/src/frontend_adapter.rs`: effective state, conversion,
  warnings, metadata/value pairing, reset lifetime, and focused tests.
- `mysql/server/src/dispatcher.rs` and `mysql/server/src/orchestrator.rs`:
  handshake character-set plumbing, preferably as a separate narrow change.

Run focused Core/session/adapter tests, then the affected crate suites and
strict clippy. The final gate must include fresh text and prepared executions,
schema reprepare, COM_STMT_RESET, NULL-first, invalid numeric text/warnings,
collations 45/46/255, alias/star projection order, and reprepare refresh.

## Not yet established

The artifacts do not establish broad conversion policy for real→text,
real→integer, text→real, blobs, ranges, decimal strings, or
`new_params_bound_flag=0` through mysql_async. Those should remain explicit
follow-up measurements rather than inferred behavior or a static
LONGLONG-for-all-markers shortcut.
