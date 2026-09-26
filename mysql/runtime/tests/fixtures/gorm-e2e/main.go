package main

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"database/sql"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	drivermysql "github.com/go-sql-driver/mysql"
	gormmysql "gorm.io/driver/mysql"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
)

type Parent struct {
	ID        int64     `gorm:"primaryKey;autoIncrement"`
	Code      string    `gorm:"size:32;not null;uniqueIndex:uk_gorm_parent_code;index:idx_gorm_shared_code"`
	CreatedAt time.Time `gorm:"precision:0"`
}

func (Parent) TableName() string { return "gorm_e2e_parents" }

type Child struct {
	ID        int64     `gorm:"primaryKey;autoIncrement"`
	ParentID  int64     `gorm:"not null;index:idx_gorm_e2e_parent"`
	Parent    Parent    `gorm:"foreignKey:ParentID;constraint:OnDelete:RESTRICT"`
	Code      string    `gorm:"size:32;not null;index:idx_gorm_shared_code"`
	Amount    string    `gorm:"type:decimal(20,6);not null"`
	Note      *string   `gorm:"size:64"`
	CreatedAt time.Time `gorm:"precision:0"`
}

func (Child) TableName() string { return "gorm_e2e_children" }

type ChildWithStatus struct {
	Child
	Status *string `gorm:"size:24"`
}

func (ChildWithStatus) TableName() string { return "gorm_e2e_children" }

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	ca, err := os.ReadFile(os.Getenv("TURSO_MYSQL_DRIVER_CA"))
	if err != nil {
		return fmt.Errorf("read CA: %w", err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(ca) {
		return fmt.Errorf("driver CA is invalid")
	}
	if err := drivermysql.RegisterTLSConfig("turso-gorm-e2e", &tls.Config{
		RootCAs: roots, ServerName: "localhost", MinVersion: tls.VersionTLS12,
	}); err != nil {
		return fmt.Errorf("register TLS: %w", err)
	}
	config := drivermysql.NewConfig()
	config.User = "gateadmin"
	config.Passwd = os.Getenv("TURSO_MYSQL_DRIVER_PASSWORD")
	config.Net = "tcp"
	config.Addr = os.Getenv("TURSO_MYSQL_DRIVER_ENDPOINT")
	config.DBName = "reports"
	config.TLSConfig = "turso-gorm-e2e"
	config.ParseTime = true
	config.Loc = time.UTC
	config.Timeout = 3 * time.Second
	open := func() (*gorm.DB, *sql.DB, error) {
		connector, err := drivermysql.NewConnector(config)
		if err != nil {
			return nil, nil, err
		}
		sqlDB := sql.OpenDB(connector)
		sqlDB.SetMaxOpenConns(2)
		gormConfig := &gorm.Config{}
		if os.Getenv("TURSO_MYSQL_GORM_TRACE") == "1" {
			gormConfig.Logger = logger.Default.LogMode(logger.Info)
		}
		db, err := gorm.Open(gormmysql.New(gormmysql.Config{Conn: sqlDB}), gormConfig)
		if err != nil {
			sqlDB.Close()
			return nil, nil, err
		}
		return db, sqlDB, nil
	}
	db, sqlDB, err := open()
	if err != nil {
		return fmt.Errorf("open GORM: %w", err)
	}
	defer sqlDB.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	db = db.WithContext(ctx)
	if err := sqlDB.PingContext(ctx); err != nil {
		return fmt.Errorf("verified TLS connection: %w", err)
	}
	if err := exercise(db); err != nil {
		return err
	}
	if err := sqlDB.Close(); err != nil {
		return fmt.Errorf("close connection pool: %w", err)
	}
	reopened, reopenedSQL, err := open()
	if err != nil {
		return fmt.Errorf("reopen GORM: %w", err)
	}
	defer reopenedSQL.Close()
	reopened = reopened.WithContext(ctx)
	if !reopened.Migrator().HasTable(&Parent{}) || !reopened.Migrator().HasTable(&Child{}) {
		return fmt.Errorf("schema missing after reopening the connection pool")
	}
	var count int64
	if err := reopened.Model(&Child{}).Count(&count).Error; err != nil || count != 1 {
		return fmt.Errorf("reopen count: count=%d err=%w", count, err)
	}
	if err := reopened.Exec("DROP TABLE gorm_e2e_children").Error; err != nil {
		return fmt.Errorf("drop child table: %w", err)
	}
	if err := reopened.Exec("DROP TABLE gorm_e2e_parents").Error; err != nil {
		return fmt.Errorf("drop parent table: %w", err)
	}
	return nil
}

func exercise(db *gorm.DB) error {
	if err := db.AutoMigrate(&Parent{}, &Child{}); err != nil {
		return fmt.Errorf("AutoMigrate initial schema: %w", err)
	}
	if err := db.AutoMigrate(&Parent{}, &Child{}); err != nil {
		return fmt.Errorf("AutoMigrate existing schema: %w", err)
	}
	if !db.Migrator().HasTable(&Parent{}) || !db.Migrator().HasTable(&Child{}) {
		return fmt.Errorf("GORM schema introspection did not find both tables")
	}
	if !db.Migrator().HasIndex(&Parent{}, "idx_gorm_shared_code") ||
		!db.Migrator().HasIndex(&Child{}, "idx_gorm_shared_code") {
		return fmt.Errorf("GORM schema introspection did not find the same index name on both tables")
	}
	for _, column := range []string{"parent_id", "amount", "note"} {
		if !db.Migrator().HasColumn(&Child{}, column) {
			return fmt.Errorf("GORM schema introspection did not find %s", column)
		}
	}
	columnTypes, err := db.Migrator().ColumnTypes(&Child{})
	if err != nil {
		return fmt.Errorf("GORM column types: %w", err)
	}
	var foundAmount bool
	for _, column := range columnTypes {
		if column.Name() == "amount" {
			precision, scale, ok := column.DecimalSize()
			if !strings.EqualFold(column.DatabaseTypeName(), "decimal") || !ok || precision != 20 || scale != 6 {
				return fmt.Errorf("wrong GORM amount type: type=%q precision=%d scale=%d known=%t", column.DatabaseTypeName(), precision, scale, ok)
			}
			foundAmount = true
		}
	}
	if !foundAmount {
		return fmt.Errorf("GORM column types omitted amount")
	}
	if err := db.Migrator().AddColumn(&ChildWithStatus{}, "Status"); err != nil {
		return fmt.Errorf("migration up: %w", err)
	}
	if !db.Migrator().HasColumn(&ChildWithStatus{}, "status") {
		return fmt.Errorf("migration up not visible to schema introspection")
	}
	if err := db.Migrator().DropColumn(&ChildWithStatus{}, "Status"); err != nil {
		return fmt.Errorf("migration down: %w", err)
	}
	if db.Migrator().HasColumn(&ChildWithStatus{}, "status") {
		return fmt.Errorf("migration down left status column")
	}
	created := time.Date(2020, time.January, 2, 3, 4, 5, 0, time.UTC)
	parent := Parent{Code: "parent-one", CreatedAt: created}
	if err := db.Create(&parent).Error; err != nil {
		return fmt.Errorf("create parent: %w", err)
	}
	if parent.ID == 0 {
		return fmt.Errorf("GORM did not receive parent auto-increment ID")
	}
	child := Child{ParentID: parent.ID, Code: "child-one", Amount: "1234567890.123456", CreatedAt: created}
	if err := db.Create(&child).Error; err != nil {
		return fmt.Errorf("create child: %w", err)
	}
	var got Child
	if err := db.Preload("Parent").First(&got, child.ID).Error; err != nil {
		return fmt.Errorf("read relation: %w", err)
	}
	if got.Parent.Code != parent.Code || got.Amount != child.Amount || got.Note != nil || !got.CreatedAt.Equal(created) {
		return fmt.Errorf("wrong relation or values: parent=%q amount=%q note=%v created=%s", got.Parent.Code, got.Amount, got.Note, got.CreatedAt)
	}
	note := "updated"
	if err := db.Model(&Child{}).Where("id = ?", child.ID).Update("note", note).Error; err != nil {
		return fmt.Errorf("update nullable field: %w", err)
	}
	if err := db.First(&got, child.ID).Error; err != nil || got.Note == nil || *got.Note != note {
		return fmt.Errorf("read updated nullable field: note=%v err=%w", got.Note, err)
	}
	if err := db.Transaction(func(tx *gorm.DB) error {
		return tx.Model(&Child{}).Where("id = ?", child.ID).Update("amount", "0.000001").Error
	}); err != nil {
		return fmt.Errorf("commit transaction: %w", err)
	}
	rollbackMarker := errors.New("roll back this transaction")
	if err := db.Transaction(func(tx *gorm.DB) error {
		if err := tx.Model(&Child{}).Where("id = ?", child.ID).Update("amount", "99.999999").Error; err != nil {
			return err
		}
		return rollbackMarker
	}); !errors.Is(err, rollbackMarker) {
		return fmt.Errorf("rollback callback: want requested rollback, got %v", err)
	}
	if err := db.First(&got, child.ID).Error; err != nil || got.Amount != "0.000001" {
		return fmt.Errorf("rollback changed exact decimal: amount=%q err=%w", got.Amount, err)
	}
	other := Child{ParentID: parent.ID, Code: "to-delete", Amount: "2.000000", CreatedAt: created}
	if err := db.Create(&other).Error; err != nil {
		return fmt.Errorf("create row for deletion: %w", err)
	}
	if result := db.Delete(&other); result.Error != nil || result.RowsAffected != 1 {
		return fmt.Errorf("delete row: affected=%d err=%w", result.RowsAffected, result.Error)
	}
	duplicate := Parent{Code: parent.Code, CreatedAt: created}
	if err := db.Create(&duplicate).Error; !mysqlError(err, 1062) {
		return fmt.Errorf("duplicate unique key: want MySQL error 1062, got %v", err)
	}
	orphan := Child{ParentID: parent.ID + 9999, Code: "orphan", Amount: "1.000000", CreatedAt: created}
	if err := db.Create(&orphan).Error; !mysqlError(err, 1452) {
		return fmt.Errorf("foreign key violation: want MySQL error 1452, got %v", err)
	}
	return nil
}

func mysqlError(err error, code uint16) bool {
	var mysqlErr *drivermysql.MySQLError
	return errors.As(err, &mysqlErr) && mysqlErr.Number == code
}
