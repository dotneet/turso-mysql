package main

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"database/sql"
	"fmt"
	"os"
	"time"

	mysql "github.com/go-sql-driver/mysql"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	ca, err := os.ReadFile(os.Getenv("TURSO_MYSQL_DRIVER_CA"))
	if err != nil {
		return err
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(ca) {
		return fmt.Errorf("driver CA is invalid")
	}
	if err := mysql.RegisterTLSConfig("turso-e2e", &tls.Config{
		RootCAs: roots, ServerName: "localhost", MinVersion: tls.VersionTLS12,
	}); err != nil {
		return err
	}
	config := mysql.NewConfig()
	config.User = "gateadmin"
	config.Passwd = os.Getenv("TURSO_MYSQL_DRIVER_PASSWORD")
	config.Net = "tcp"
	config.Addr = os.Getenv("TURSO_MYSQL_DRIVER_ENDPOINT")
	config.DBName = "reports"
	config.TLSConfig = "turso-e2e"
	config.MultiStatements = true
	config.Timeout = 3 * time.Second
	connector, err := mysql.NewConnector(config)
	if err != nil {
		return err
	}
	db := sql.OpenDB(connector)
	defer db.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := db.PingContext(ctx); err != nil {
		return fmt.Errorf("connect: %w", err)
	}
	var packetLimit int64
	if err := db.QueryRowContext(ctx, "SELECT @@max_allowed_packet").Scan(&packetLimit); err != nil {
		return fmt.Errorf("server packet limit: %w", err)
	}
	if packetLimit <= 0 {
		return fmt.Errorf("invalid server packet limit: %d", packetLimit)
	}
	multiple, err := db.QueryContext(ctx, "SELECT 1; SELECT 2")
	if err != nil {
		return fmt.Errorf("multi-statement query: %w", err)
	}
	for expected := int64(1); expected <= 2; expected++ {
		if !multiple.Next() {
			multiple.Close()
			return fmt.Errorf("multi-statement result %d is empty", expected)
		}
		var actual int64
		if err := multiple.Scan(&actual); err != nil || actual != expected {
			multiple.Close()
			return fmt.Errorf("multi-statement result %d: %d, %v", expected, actual, err)
		}
		if multiple.Next() {
			multiple.Close()
			return fmt.Errorf("multi-statement result %d has extra rows", expected)
		}
		if expected < 2 && !multiple.NextResultSet() {
			multiple.Close()
			return fmt.Errorf("missing next result set")
		}
	}
	if multiple.NextResultSet() || multiple.Err() != nil {
		multiple.Close()
		return fmt.Errorf("unexpected trailing result set: %v", multiple.Err())
	}
	multiple.Close()
	if _, err := db.ExecContext(ctx, "CREATE TABLE go_records (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) NOT NULL)"); err != nil {
		return fmt.Errorf("create table: %w", err)
	}
	stmt, err := db.PrepareContext(ctx, "INSERT INTO go_records (id, name) VALUES (?, ?)")
	if err != nil {
		return fmt.Errorf("prepare insert: %w", err)
	}
	defer stmt.Close()
	if _, err := stmt.ExecContext(ctx, 1, "alpha"); err != nil {
		return fmt.Errorf("insert: %w", err)
	}
	var name string
	if err := db.QueryRowContext(ctx, "SELECT name FROM go_records WHERE id = 1").Scan(&name); err != nil {
		return fmt.Errorf("select: %w", err)
	}
	if name != "alpha" {
		return fmt.Errorf("unexpected name: %q", name)
	}
	tx, err := db.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("begin: %w", err)
	}
	if _, err := tx.ExecContext(ctx, "UPDATE go_records SET name = 'beta' WHERE id = 1"); err != nil {
		return fmt.Errorf("update: %w", err)
	}
	if err := tx.Rollback(); err != nil {
		return fmt.Errorf("rollback: %w", err)
	}
	if err := db.QueryRowContext(ctx, "SELECT name FROM go_records WHERE id = 1").Scan(&name); err != nil || name != "alpha" {
		return fmt.Errorf("rollback did not restore row: %q, %v", name, err)
	}
	if _, err := db.ExecContext(ctx, "ALTER TABLE go_records ADD COLUMN note VARCHAR(20)"); err != nil {
		return fmt.Errorf("migration: %w", err)
	}
	rows, err := db.QueryContext(ctx,
		"SELECT COLUMN_NAME, CHARACTER_MAXIMUM_LENGTH FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'go_records' ORDER BY ORDINAL_POSITION")
	if err != nil {
		return fmt.Errorf("schema query: %w", err)
	}
	var foundName, foundNote bool
	for rows.Next() {
		var column string
		var length sql.NullInt64
		if err := rows.Scan(&column, &length); err != nil {
			rows.Close()
			return fmt.Errorf("schema row: %w", err)
		}
		if column == "name" {
			foundName = length.Valid && length.Int64 == 20
		}
		if column == "note" {
			foundNote = length.Valid && length.Int64 == 20
		}
	}
	if err := rows.Err(); err != nil {
		rows.Close()
		return err
	}
	rows.Close()
	if !foundName || !foundNote {
		return fmt.Errorf("schema query did not report migrated varchar columns")
	}
	deleted, err := db.ExecContext(ctx, "DELETE FROM go_records WHERE id = 1")
	if err != nil {
		return fmt.Errorf("delete: %w", err)
	}
	if count, err := deleted.RowsAffected(); err != nil || count != 1 {
		return fmt.Errorf("delete count: %d, %v", count, err)
	}
	if _, err := db.ExecContext(ctx, "DROP TABLE go_records"); err != nil {
		return fmt.Errorf("drop table: %w", err)
	}
	if _, err := db.ExecContext(ctx, "CREATE TABLE go_values (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) UNIQUE, amount DECIMAL(65,30))"); err != nil {
		return fmt.Errorf("create exact values table: %w", err)
	}
	const exactDecimal = "1.234567890123456789012345678901"
	if _, err := db.ExecContext(ctx, "INSERT INTO go_values (id, name, amount) VALUES (?, ?, ?)", 1, "café", exactDecimal); err != nil {
		return fmt.Errorf("insert exact decimal: %w", err)
	}
	var amount string
	if err := db.QueryRowContext(ctx, "SELECT amount FROM go_values WHERE name = ?", "CAFE").Scan(&amount); err != nil {
		return fmt.Errorf("select exact decimal using Unicode collation: %w", err)
	}
	if amount != exactDecimal {
		return fmt.Errorf("wrong exact decimal: %q", amount)
	}
	if _, err := db.ExecContext(ctx, "DROP TABLE go_values"); err != nil {
		return fmt.Errorf("drop exact values table: %w", err)
	}
	return nil
}
