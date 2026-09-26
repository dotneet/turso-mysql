# mysqldump roundtrip fixture

`run.sh` starts a disposable MySQL 8.4.11 sidecar and runs the cross-UID TLS
fixture with pinned mysqldump 8.0.46. The test restores the unmodified dump
in both directions: Turso to MySQL and MySQL to Turso. It checks table rows,
DECIMAL and NULL values, a view, and a trigger after restore.
The disposable MySQL server enables `log_bin_trust_function_creators` so the
unmodified dump can restore its trigger with the fixture's database-scoped
account while binary logging is enabled.

The dump uses `--lock-tables`. The `--single-transaction` mode is not supported
by this invocation: mysqldump sends `START TRANSACTION ... WITH CONSISTENT
SNAPSHOT` before selecting its positional database, while Turso needs a
selected database to begin a transaction. Turso returns 1046 instead of
claiming a snapshot it did not create. A MySQL 8.4.11 general log confirmed
that `--databases` does not change this command order. The test also uses
`--skip-network-timeout`, because mysqldump otherwise requests 86400-second
session network timeouts that this server does not provide.
