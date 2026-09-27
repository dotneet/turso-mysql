// A small GORM app run step by step against a MySQL-compatible server. Every
// step appends {"step","ok","error"} to $E2E_OUT/steps.jsonl and never stops
// the run; GORM's own logger prints every statement to stdout.
package main

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"regexp"
	"runtime/debug"
	"sort"
	"strings"
	"sync"
	"time"

	mysqldriver "github.com/go-sql-driver/mysql"
	"gorm.io/datatypes"
	"gorm.io/driver/mysql"
	"gorm.io/gorm"
	"gorm.io/gorm/clause"
	"gorm.io/gorm/logger"
)

type User struct {
	ID        uint64         `gorm:"primaryKey;autoIncrement"`
	Email     string         `gorm:"type:varchar(191);not null;uniqueIndex"`
	Name      string         `gorm:"type:varchar(100);not null"`
	Balance   float64        `gorm:"type:decimal(10,2);not null;default:0.00"`
	IsActive  bool           `gorm:"not null"`
	Profile   datatypes.JSON `gorm:"type:json"`
	CreatedAt time.Time
	UpdatedAt time.Time
	Posts     []Post `gorm:"foreignKey:UserID;constraint:OnDelete:CASCADE"`
}

type Post struct {
	ID          uint64 `gorm:"primaryKey;autoIncrement"`
	UserID      uint64 `gorm:"not null;index"`
	User        *User
	Title       string `gorm:"type:varchar(200);not null"`
	Body        string `gorm:"type:text"`
	PublishedAt *time.Time
	Views       int   `gorm:"type:int;not null;default:0"`
	Tags        []Tag `gorm:"many2many:post_tags"`
	CreatedAt   time.Time
	UpdatedAt   time.Time
}

type Tag struct {
	ID   uint64 `gorm:"primaryKey;autoIncrement"`
	Name string `gorm:"type:varchar(64);not null;uniqueIndex"`
}

// PostV2 is the posts table after the second migration. Content is here so
// that RenameColumn can fall back to CHANGE on servers older than MySQL 8.0.
type PostV2 struct {
	ID      uint64  `gorm:"primaryKey;autoIncrement"`
	Slug    *string `gorm:"type:varchar(200);index:idx_posts_slug"`
	Content string  `gorm:"type:text"`
	Views   int64   `gorm:"not null;default:0"`
}

func (PostV2) TableName() string { return "posts" }

func main() {
	app := newApp()
	app.step("connect-default-charset", app.connectDefaultCharset)
	app.step("connect", app.connect)
	app.step("migrate", app.migrate)
	app.step("migrate-again", app.migrateAgain)
	app.step("introspect", app.introspect)
	app.step("insert", app.insert)
	app.step("create-in-batches", app.createInBatches)
	app.step("associations", app.associations)
	app.step("relations", app.relations)
	app.step("update", app.update)
	app.step("delete", app.delete)
	app.step("pagination", app.pagination)
	app.step("aggregate", app.aggregate)
	app.step("transaction-commit", app.transactionCommit)
	app.step("transaction-rollback", app.transactionRollback)
	app.step("savepoint", app.savepoint)
	app.step("json", app.json)
	app.step("upsert", app.upsert)
	app.step("alter-migration", app.alterMigration)
	app.step("rollback-migration", app.rollbackMigration)
	app.step("drop-tables", app.dropTables)
}

type app struct {
	db     *gorm.DB
	sql    *statementLog
	stepsF *os.File
}

func newApp() *app {
	out := os.Getenv("E2E_OUT")
	f, err := os.OpenFile(out+"/steps.jsonl", os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		panic(err)
	}
	return &app{stepsF: f, sql: &statementLog{}}
}

func (a *app) step(name string, fn func() error) {
	fmt.Printf("=== step %s at %s\n", name, time.Now().UTC().Format(time.RFC3339Nano))
	err := runRecovering(fn)
	rec := map[string]any{"step": name, "ok": err == nil}
	if err != nil {
		msg := err.Error()
		fmt.Printf("step %s FAILED: %s\n", name, msg)
		if len(msg) > 2000 {
			msg = msg[len(msg)-2000:]
		}
		rec["error"] = msg
	}
	line, _ := json.Marshal(rec)
	a.stepsF.Write(append(line, '\n'))
}

func runRecovering(fn func() error) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic: %v\n%s", r, debug.Stack())
		}
	}()
	return fn()
}

func dsn(params string) string {
	return fmt.Sprintf("%s:%s@tcp(%s:%s)/%s?tls=e2e%s",
		os.Getenv("E2E_USER"), os.Getenv("E2E_PASSWORD"), os.Getenv("E2E_HOST"),
		os.Getenv("E2E_PORT"), os.Getenv("E2E_APP"), params)
}

func registerTLS() error {
	pem, err := os.ReadFile(os.Getenv("E2E_CA"))
	if err != nil {
		return err
	}
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(pem) {
		return errors.New("no certificate in E2E_CA")
	}
	return mysqldriver.RegisterTLSConfig("e2e", &tls.Config{RootCAs: pool, ServerName: os.Getenv("E2E_HOST")})
}

// With no charset parameter go-sql-driver sends its default collation in the
// handshake and issues no SET NAMES.
func (a *app) connectDefaultCharset() error {
	if err := registerTLS(); err != nil {
		return err
	}
	db, err := gorm.Open(mysql.Open(dsn("&parseTime=True")), &gorm.Config{Logger: a.newLogger()})
	if err != nil {
		return err
	}
	sqlDB, _ := db.DB()
	defer sqlDB.Close()
	var row struct {
		Client     string
		Collation  string
		Connection string
	}
	if err := db.Raw("SELECT @@character_set_client AS client, @@collation_connection AS collation, " +
		"@@character_set_connection AS connection").Scan(&row).Error; err != nil {
		return err
	}
	fmt.Printf("default charset: client=%s connection=%s collation=%s\n", row.Client, row.Connection, row.Collation)
	return nil
}

func (a *app) connect() error {
	db, err := gorm.Open(mysql.Open(dsn("&charset=utf8mb4&parseTime=True&loc=UTC")), &gorm.Config{Logger: a.newLogger()})
	if err != nil {
		return err
	}
	a.db = db
	var version string
	if err := db.Raw("SELECT VERSION()").Scan(&version).Error; err != nil {
		return err
	}
	fmt.Println("server version:", version)
	return nil
}

func (a *app) newLogger() logger.Interface {
	return &capturingLogger{
		Interface: logger.New(log.New(os.Stdout, "", log.LstdFlags), logger.Config{LogLevel: logger.Info, Colorful: false}),
		log:       a.sql,
	}
}

func (a *app) migrate() error {
	return a.db.AutoMigrate(&User{}, &Post{}, &Tag{})
}

var schemaChange = regexp.MustCompile(`(?i)^\s*(CREATE|ALTER|DROP|RENAME)\b`)

func (a *app) migrateAgain() error {
	a.sql.start()
	err := a.db.AutoMigrate(&User{}, &Post{}, &Tag{})
	statements := a.sql.stop()
	if err != nil {
		return err
	}
	var changes []string
	for _, s := range statements {
		if schemaChange.MatchString(s) {
			changes = append(changes, s)
		}
	}
	if len(changes) > 0 {
		return fmt.Errorf("second AutoMigrate changed the schema:\n%s", strings.Join(changes, "\n"))
	}
	return nil
}

func (a *app) introspect() error {
	m := a.db.Migrator()
	tables, err := m.GetTables()
	if err != nil {
		return err
	}
	sort.Strings(tables)
	fmt.Println("tables:", tables)
	if want := []string{"post_tags", "posts", "tags", "users"}; fmt.Sprint(tables) != fmt.Sprint(want) {
		return fmt.Errorf("GetTables = %v, want %v", tables, want)
	}
	for _, model := range []any{&User{}, &Post{}, &Tag{}} {
		if !m.HasTable(model) {
			return fmt.Errorf("HasTable(%T) = false", model)
		}
	}
	columns, err := m.ColumnTypes(&User{})
	if err != nil {
		return err
	}
	byName := map[string]gorm.ColumnType{}
	for _, c := range columns {
		byName[c.Name()] = c
		ct, _ := c.ColumnType()
		nullable, _ := c.Nullable()
		pk, _ := c.PrimaryKey()
		unique, _ := c.Unique()
		auto, _ := c.AutoIncrement()
		def, hasDef := c.DefaultValue()
		length, _ := c.Length()
		precision, scale, _ := c.DecimalSize()
		fmt.Printf("users.%s: type=%s db=%s null=%v pk=%v unique=%v auto=%v default=%q(%v) length=%d decimal=%d,%d\n",
			c.Name(), ct, c.DatabaseTypeName(), nullable, pk, unique, auto, def, hasDef, length, precision, scale)
	}
	if len(columns) != 8 {
		return fmt.Errorf("users has %d columns, want 8", len(columns))
	}
	if p, s, _ := byName["balance"].DecimalSize(); p != 10 || s != 2 {
		return fmt.Errorf("users.balance decimal size = %d,%d, want 10,2", p, s)
	}
	if pk, _ := byName["id"].PrimaryKey(); !pk {
		return errors.New("users.id is not reported as the primary key")
	}
	if auto, _ := byName["id"].AutoIncrement(); !auto {
		return errors.New("users.id is not reported as auto increment")
	}
	for _, model := range []any{&User{}, &Post{}, &Tag{}} {
		indexes, err := m.GetIndexes(model)
		if err != nil {
			return err
		}
		for _, idx := range indexes {
			unique, _ := idx.Unique()
			pk, _ := idx.PrimaryKey()
			fmt.Printf("%T index %s on %v unique=%v primary=%v\n", model, idx.Name(), idx.Columns(), unique, pk)
		}
	}
	if !m.HasIndex(&User{}, "Email") {
		return errors.New("HasIndex(User, Email) = false")
	}
	if !m.HasIndex(&Post{}, "idx_posts_user_id") {
		return errors.New("HasIndex(Post, idx_posts_user_id) = false")
	}
	if !m.HasConstraint(&User{}, "Posts") {
		return errors.New("HasConstraint(User, Posts) = false")
	}
	if !m.HasConstraint("post_tags", "fk_post_tags_post") {
		return errors.New("HasConstraint(post_tags, fk_post_tags_post) = false")
	}
	if !m.HasColumn(&Post{}, "published_at") {
		return errors.New("HasColumn(Post, published_at) = false")
	}
	return nil
}

func (a *app) insert() error {
	alice := User{Email: "alice@example.com", Name: "Alice", Balance: 100.50, IsActive: true,
		Profile: datatypes.JSON(`{"city": "Paris", "tags": ["a", "b"]}`),
		Posts: []Post{
			{Title: "Hello", Body: "first post", Views: 10, Tags: []Tag{{Name: "go"}, {Name: "sql"}}},
			{Title: "Again", Body: "second post", Views: 5, PublishedAt: ptr(time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC))},
		}}
	if err := a.db.Create(&alice).Error; err != nil {
		return err
	}
	bob := User{Email: "bob@example.com", Name: "Bob", Balance: 20, IsActive: false,
		Profile: datatypes.JSON(`{"city": "Berlin"}`),
		Posts:   []Post{{Title: "Bob's post", Body: "text", Views: 1}}}
	if err := a.db.Create(&bob).Error; err != nil {
		return err
	}
	carol := User{Email: "carol@example.com", Name: "Carol", Balance: 0, IsActive: true}
	if err := a.db.Create(&carol).Error; err != nil {
		return err
	}
	if alice.ID == 0 || bob.ID == 0 || carol.ID == 0 || alice.Posts[0].ID == 0 {
		return fmt.Errorf("ids not filled in: alice=%d bob=%d carol=%d post=%d", alice.ID, bob.ID, carol.ID, alice.Posts[0].ID)
	}
	var got User
	if err := a.db.First(&got, "email = ?", "alice@example.com").Error; err != nil {
		return err
	}
	if got.Name != "Alice" || got.Balance != 100.50 || !got.IsActive || got.CreatedAt.IsZero() {
		return fmt.Errorf("read back %+v", got)
	}
	return nil
}

func (a *app) createInBatches() error {
	var tags []Tag
	for i := 0; i < 7; i++ {
		tags = append(tags, Tag{Name: fmt.Sprintf("batch-%d", i)})
	}
	if err := a.db.CreateInBatches(&tags, 3).Error; err != nil {
		return err
	}
	for _, t := range tags {
		if t.ID == 0 {
			return fmt.Errorf("tag %s got no id", t.Name)
		}
	}
	var n int64
	if err := a.db.Model(&Tag{}).Where("name LIKE ?", "batch-%").Count(&n).Error; err != nil {
		return err
	}
	if n != 7 {
		return fmt.Errorf("batch tags = %d, want 7", n)
	}
	return nil
}

func (a *app) associations() error {
	var post Post
	if err := a.db.Where("title = ?", "Again").First(&post).Error; err != nil {
		return err
	}
	var goTag, batch0, batch1 Tag
	if err := a.db.Where("name = ?", "go").First(&goTag).Error; err != nil {
		return err
	}
	a.db.Where("name = ?", "batch-0").First(&batch0)
	a.db.Where("name = ?", "batch-1").First(&batch1)
	if err := a.db.Model(&post).Association("Tags").Append(&goTag, &Tag{Name: "news"}); err != nil {
		return err
	}
	if n := a.db.Model(&post).Association("Tags").Count(); n != 2 {
		return fmt.Errorf("after Append the post has %d tags, want 2", n)
	}
	if err := a.db.Model(&post).Association("Tags").Replace(&batch0, &batch1); err != nil {
		return err
	}
	var names []string
	var tags []Tag
	if err := a.db.Model(&post).Association("Tags").Find(&tags); err != nil {
		return err
	}
	for _, t := range tags {
		names = append(names, t.Name)
	}
	sort.Strings(names)
	if fmt.Sprint(names) != "[batch-0 batch-1]" {
		return fmt.Errorf("after Replace the tags are %v", names)
	}
	return nil
}

func (a *app) relations() error {
	var users []User
	if err := a.db.Preload("Posts", func(db *gorm.DB) *gorm.DB { return db.Order("posts.id") }).
		Preload("Posts.Tags").Order("id").Find(&users).Error; err != nil {
		return err
	}
	if len(users) != 3 {
		return fmt.Errorf("preload returned %d users, want 3", len(users))
	}
	if alice := users[0]; len(alice.Posts) != 2 || len(alice.Posts[0].Tags) != 2 {
		return fmt.Errorf("preload returned alice with %+v", alice.Posts)
	}
	var posts []Post
	if err := a.db.Joins("User").Where("User.is_active = ?", true).Order("posts.id").Find(&posts).Error; err != nil {
		return err
	}
	if len(posts) != 2 || posts[0].User == nil || posts[0].User.Name != "Alice" {
		return fmt.Errorf("joins returned %d posts: %+v", len(posts), posts)
	}
	var rows []struct {
		Name  string
		Title string
		Tag   string
	}
	if err := a.db.Table("posts").
		Select("users.name, posts.title, tags.name AS tag").
		Joins("JOIN users ON users.id = posts.user_id").
		Joins("JOIN post_tags ON post_tags.post_id = posts.id").
		Joins("JOIN tags ON tags.id = post_tags.tag_id").
		Order("posts.id, tags.name").Scan(&rows).Error; err != nil {
		return err
	}
	if len(rows) != 4 {
		return fmt.Errorf("explicit join returned %d rows: %+v", len(rows), rows)
	}
	return nil
}

func (a *app) update() error {
	var u User
	if err := a.db.First(&u, "email = ?", "bob@example.com").Error; err != nil {
		return err
	}
	before := u.UpdatedAt
	time.Sleep(10 * time.Millisecond)
	if err := a.db.Model(&u).Update("name", "Robert").Error; err != nil {
		return err
	}
	if err := a.db.Model(&u).Updates(map[string]any{"balance": 25.75, "is_active": true}).Error; err != nil {
		return err
	}
	if err := a.db.Model(&Post{}).Where("user_id = ?", u.ID).
		UpdateColumn("views", gorm.Expr("views + ?", 3)).Error; err != nil {
		return err
	}
	if err := a.db.First(&u, u.ID).Error; err != nil {
		return err
	}
	u.Name = "Rob"
	if err := a.db.Save(&u).Error; err != nil {
		return err
	}
	var got User
	if err := a.db.Preload("Posts").First(&got, u.ID).Error; err != nil {
		return err
	}
	if got.Name != "Rob" || got.Balance != 25.75 || !got.IsActive || got.Posts[0].Views != 4 || !got.UpdatedAt.After(before) {
		return fmt.Errorf("after update: %+v", got)
	}
	return nil
}

func (a *app) delete() error {
	var carol User
	if err := a.db.First(&carol, "email = ?", "carol@example.com").Error; err != nil {
		return err
	}
	post := Post{UserID: carol.ID, Title: "to cascade", Tags: []Tag{{Name: "doomed"}}}
	if err := a.db.Create(&post).Error; err != nil {
		return err
	}
	if err := a.db.Model(&post).Association("Tags").Clear(); err != nil {
		return err
	}
	if err := a.db.Delete(&carol).Error; err != nil {
		return err
	}
	var n int64
	if err := a.db.Model(&Post{}).Where("user_id = ?", carol.ID).Count(&n).Error; err != nil {
		return err
	}
	if n != 0 {
		return fmt.Errorf("ON DELETE CASCADE left %d posts", n)
	}
	if err := a.db.Where("name = ?", "doomed").Delete(&Tag{}).Error; err != nil {
		return err
	}
	res := a.db.Delete(&User{}, "email = ?", "nobody@example.com")
	if res.Error != nil || res.RowsAffected != 0 {
		return fmt.Errorf("delete of a missing row: %v, %d rows", res.Error, res.RowsAffected)
	}
	return nil
}

func (a *app) pagination() error {
	for i := 0; i < 5; i++ {
		if err := a.db.Create(&Post{UserID: firstUserID(a.db), Title: fmt.Sprintf("page %d", i)}).Error; err != nil {
			return err
		}
	}
	var total int64
	if err := a.db.Model(&Post{}).Count(&total).Error; err != nil {
		return err
	}
	var page []Post
	if err := a.db.Order("id").Limit(2).Offset(2).Find(&page).Error; err != nil {
		return err
	}
	if total != 8 || len(page) != 2 {
		return fmt.Errorf("total=%d, page has %d rows", total, len(page))
	}
	var titles []string
	if err := a.db.Model(&Post{}).Where("title LIKE ?", "page %").Order("id DESC").Limit(3).Pluck("title", &titles).Error; err != nil {
		return err
	}
	if fmt.Sprint(titles) != "[page 4 page 3 page 2]" {
		return fmt.Errorf("pluck = %v", titles)
	}
	return nil
}

func firstUserID(db *gorm.DB) uint64 {
	var u User
	db.Order("id").First(&u)
	return u.ID
}

func (a *app) aggregate() error {
	var rows []struct {
		UserID  uint64
		N       int64
		Total   int64
		Average float64
	}
	if err := a.db.Model(&Post{}).
		Select("user_id, COUNT(*) AS n, SUM(views) AS total, AVG(views) AS average").
		Group("user_id").Having("COUNT(*) > ?", 1).Order("user_id").Scan(&rows).Error; err != nil {
		return err
	}
	if len(rows) != 1 || rows[0].N != 7 || rows[0].Total != 15 {
		return fmt.Errorf("group by: %+v", rows)
	}
	var sum struct {
		Total float64
		Avg   float64
		Max   float64
	}
	if err := a.db.Model(&User{}).Select("SUM(balance) AS total, AVG(balance) AS avg, MAX(balance) AS max").
		Where("is_active = ?", true).Scan(&sum).Error; err != nil {
		return err
	}
	if sum.Total != 126.25 || sum.Max != 100.5 {
		return fmt.Errorf("sums: %+v", sum)
	}
	var distinct int64
	if err := a.db.Model(&Post{}).Distinct("user_id").Count(&distinct).Error; err != nil {
		return err
	}
	if distinct != 2 {
		return fmt.Errorf("distinct user_id count = %d", distinct)
	}
	return nil
}

func (a *app) transactionCommit() error {
	err := a.db.Transaction(func(tx *gorm.DB) error {
		if err := tx.Model(&User{}).Where("email = ?", "alice@example.com").
			UpdateColumn("balance", gorm.Expr("balance - ?", 10)).Error; err != nil {
			return err
		}
		return tx.Model(&User{}).Where("email = ?", "bob@example.com").
			UpdateColumn("balance", gorm.Expr("balance + ?", 10)).Error
	})
	if err != nil {
		return err
	}
	var u User
	a.db.First(&u, "email = ?", "bob@example.com")
	if u.Balance != 35.75 {
		return fmt.Errorf("bob's balance after commit = %v", u.Balance)
	}
	return nil
}

var errRollback = errors.New("roll back on purpose")

func (a *app) transactionRollback() error {
	err := a.db.Transaction(func(tx *gorm.DB) error {
		if err := tx.Create(&Tag{Name: "rolled-back"}).Error; err != nil {
			return err
		}
		return errRollback
	})
	if !errors.Is(err, errRollback) {
		return fmt.Errorf("transaction returned %v", err)
	}
	var n int64
	if err := a.db.Model(&Tag{}).Where("name = ?", "rolled-back").Count(&n).Error; err != nil {
		return err
	}
	if n != 0 {
		return errors.New("the rolled back tag exists")
	}
	return nil
}

func (a *app) savepoint() error {
	err := a.db.Transaction(func(tx *gorm.DB) error {
		if err := tx.Create(&Tag{Name: "outer"}).Error; err != nil {
			return err
		}
		nested := tx.Transaction(func(tx2 *gorm.DB) error {
			if err := tx2.Create(&Tag{Name: "nested"}).Error; err != nil {
				return err
			}
			return errRollback
		})
		if !errors.Is(nested, errRollback) {
			return fmt.Errorf("nested transaction returned %v", nested)
		}
		if err := tx.SavePoint("sp_manual").Error; err != nil {
			return err
		}
		if err := tx.Create(&Tag{Name: "manual"}).Error; err != nil {
			return err
		}
		return tx.RollbackTo("sp_manual").Error
	})
	if err != nil {
		return err
	}
	var names []string
	if err := a.db.Model(&Tag{}).Where("name IN ?", []string{"outer", "nested", "manual"}).Pluck("name", &names).Error; err != nil {
		return err
	}
	if fmt.Sprint(names) != "[outer]" {
		return fmt.Errorf("after savepoints the tags are %v, want [outer]", names)
	}
	return nil
}

func (a *app) json() error {
	var users []User
	if err := a.db.Where(datatypes.JSONQuery("profile").Equals("Paris", "city")).Find(&users).Error; err != nil {
		return err
	}
	if len(users) != 1 || users[0].Name != "Alice" {
		return fmt.Errorf("JSONQuery Equals found %d users", len(users))
	}
	if err := a.db.Where(datatypes.JSONQuery("profile").HasKey("tags")).Find(&users).Error; err != nil {
		return err
	}
	if len(users) != 1 {
		return fmt.Errorf("JSONQuery HasKey found %d users", len(users))
	}
	var cities []string
	if err := a.db.Model(&User{}).Where("JSON_EXTRACT(profile, '$.city') IS NOT NULL").Order("id").
		Pluck("JSON_UNQUOTE(JSON_EXTRACT(profile, '$.city'))", &cities).Error; err != nil {
		return err
	}
	if fmt.Sprint(cities) != "[Paris Berlin]" {
		return fmt.Errorf("cities = %v", cities)
	}
	var u User
	if err := a.db.First(&u, "email = ?", "alice@example.com").Error; err != nil {
		return err
	}
	var profile map[string]any
	if err := json.Unmarshal(u.Profile, &profile); err != nil || profile["city"] != "Paris" {
		return fmt.Errorf("profile read back as %s (%v)", u.Profile, err)
	}
	return nil
}

func (a *app) upsert() error {
	users := []User{
		{Email: "alice@example.com", Name: "Alice Updated", Balance: 1},
		{Email: "dave@example.com", Name: "Dave", Balance: 2, IsActive: true},
	}
	if err := a.db.Clauses(clause.OnConflict{
		Columns:   []clause.Column{{Name: "email"}},
		DoUpdates: clause.AssignmentColumns([]string{"name", "balance"}),
	}).Create(&users).Error; err != nil {
		return err
	}
	if err := a.db.Clauses(clause.OnConflict{DoNothing: true}).Create(&Tag{Name: "go"}).Error; err != nil {
		return err
	}
	if err := a.db.Clauses(clause.OnConflict{UpdateAll: true}).
		Create(&Tag{Name: "sql"}).Error; err != nil {
		return err
	}
	var alice User
	a.db.First(&alice, "email = ?", "alice@example.com")
	var count int64
	a.db.Model(&User{}).Count(&count)
	if alice.Name != "Alice Updated" || alice.Balance != 1 || count != 3 {
		return fmt.Errorf("after upsert alice=%+v users=%d", alice, count)
	}
	var tags int64
	a.db.Model(&Tag{}).Where("name IN ?", []string{"go", "sql"}).Count(&tags)
	if tags != 2 {
		return fmt.Errorf("go/sql tags = %d, want 2", tags)
	}
	return nil
}

func (a *app) alterMigration() error {
	m := a.db.Migrator()
	if err := m.AddColumn(&PostV2{}, "Slug"); err != nil {
		return err
	}
	if err := m.CreateIndex(&PostV2{}, "idx_posts_slug"); err != nil {
		return err
	}
	if err := m.RenameColumn(&PostV2{}, "body", "content"); err != nil {
		return err
	}
	if err := m.AlterColumn(&PostV2{}, "Views"); err != nil {
		return err
	}
	if !m.HasColumn(&PostV2{}, "slug") || !m.HasIndex(&PostV2{}, "idx_posts_slug") || !m.HasColumn(&PostV2{}, "content") {
		return errors.New("the new column, index or renamed column is missing")
	}
	columns, err := m.ColumnTypes(&PostV2{})
	if err != nil {
		return err
	}
	for _, c := range columns {
		if c.Name() == "views" && !strings.EqualFold(c.DatabaseTypeName(), "bigint") {
			return fmt.Errorf("views is %s after AlterColumn, want bigint", c.DatabaseTypeName())
		}
	}
	return a.db.Exec("UPDATE posts SET slug = CONCAT('post-', id)").Error
}

func (a *app) rollbackMigration() error {
	m := a.db.Migrator()
	if err := m.DropIndex(&PostV2{}, "idx_posts_slug"); err != nil {
		return err
	}
	if err := m.DropColumn(&PostV2{}, "Slug"); err != nil {
		return err
	}
	if err := m.RenameColumn(&PostV2{}, "content", "body"); err != nil {
		return err
	}
	if m.HasColumn(&PostV2{}, "slug") || m.HasIndex(&PostV2{}, "idx_posts_slug") || !m.HasColumn(&Post{}, "body") {
		return errors.New("the rollback left the schema changed")
	}
	return nil
}

func (a *app) dropTables() error {
	m := a.db.Migrator()
	if err := m.DropTable("post_tags", &Post{}, &Tag{}, &User{}); err != nil {
		return err
	}
	tables, err := m.GetTables()
	if err != nil {
		return err
	}
	if len(tables) != 0 {
		return fmt.Errorf("tables left: %v", tables)
	}
	return nil
}

func ptr[T any](v T) *T { return &v }

// capturingLogger prints like GORM's default logger and, between start and
// stop, keeps every statement so a step can check what was sent.
type capturingLogger struct {
	logger.Interface
	log *statementLog
}

func (l *capturingLogger) LogMode(level logger.LogLevel) logger.Interface {
	return &capturingLogger{Interface: l.Interface.LogMode(level), log: l.log}
}

func (l *capturingLogger) Trace(ctx context.Context, begin time.Time, fc func() (string, int64), err error) {
	sql, rows := fc()
	l.log.add(sql)
	l.Interface.Trace(ctx, begin, func() (string, int64) { return sql, rows }, err)
}

type statementLog struct {
	mu        sync.Mutex
	capturing bool
	sql       []string
}

func (s *statementLog) start() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.capturing, s.sql = true, nil
}

func (s *statementLog) stop() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.capturing = false
	return s.sql
}

func (s *statementLog) add(sql string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.capturing {
		s.sql = append(s.sql, sql)
	}
}
