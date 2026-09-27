-- The e2e account creates and drops each framework's database, like the
-- per-database create/drop grants the turso account receives.
GRANT ALL PRIVILEGES ON *.* TO 'e2e'@'%';
