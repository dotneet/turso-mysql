// Entity Framework Core 9 with Pomelo.EntityFrameworkCore.MySql used the way
// an ASP.NET app does: `dotnet ef database update` for migrations (and back
// down), `dotnet ef dbcontext scaffold` for database-first introspection,
// LINQ with Include, GroupBy and ExecuteUpdate, a DateTime row version for
// optimistic concurrency, transactions with savepoints, raw SQL, and
// EnsureDeleted/EnsureCreated. Every step is recorded in
// $E2E_OUT/steps.jsonl and a failing step never stops the run.
using System.Data;
using System.Diagnostics;
using System.Text.Json;
using E2e;
using Microsoft.EntityFrameworkCore;

var options = new Lazy<DbContextOptions<BlogContext>>(() => BlogContext.Options());
BlogContext Db() => new(options.Value);

Step("connect", () =>
{
    using var db = Db();
    var row = db.Database.SqlQuery<string>($"SELECT CONCAT(VERSION(), ' ', DATABASE()) AS Value").Single();
    Console.WriteLine(row);
    Expect(row.EndsWith(" " + Environment.GetEnvironmentVariable("E2E_APP")), $"connected to {row}");
    Expect(db.Database.CanConnect(), "CanConnect is false");
});

Step("migrate", () =>
{
    Ef("database", "update", "CreateBlog");
    using var db = Db();
    var tables = TableNames(db);
    foreach (var t in new[] { "__EFMigrationsHistory", "Users", "Posts", "Tags", "PostTags" })
        Expect(tables.Contains(t), $"table {t} is missing from {string.Join(",", tables)}");
});

Step("migrations-list", () =>
{
    var output = Ef("migrations", "list");
    Expect(output.Contains("_CreateBlog") && !output.Contains("_CreateBlog (Pending)"), "CreateBlog is not applied");
    Expect(output.Contains("_AddSlugAndRenameViews (Pending)"), "AddSlugAndRenameViews is not pending");
    using var db = Db();
    Expect(db.Database.GetAppliedMigrations().Count() == 1, "GetAppliedMigrations");
});

Step("scaffold", () =>
{
    var dir = "/tmp/scaffold";
    if (Directory.Exists(dir)) Directory.Delete(dir, true);
    Ef("dbcontext", "scaffold", BlogContext.ConnectionString(), "Pomelo.EntityFrameworkCore.MySql",
        "--output-dir", dir, "--context", "ScaffoldedContext", "--no-onconfiguring", "--force");
    var user = File.ReadAllText(Path.Combine(dir, "User.cs"));
    var context = File.ReadAllText(Path.Combine(dir, "ScaffoldedContext.cs"));
    Console.WriteLine(context);
    Expect(user.Contains("public string Email") && user.Contains("public decimal Balance") && user.Contains("public virtual ICollection<Post> Posts"),
        "scaffolded User.cs is wrong:\n" + user);
    Expect(context.Contains(".HasMaxLength(191)") && context.Contains("HasColumnType(\"json\")") && context.Contains("IsUnique()")
        && context.Contains("HasForeignKey(d => d.UserId)") && context.Contains("j.HasKey(\"PostsId\", \"TagsId\")") && context.Contains("j.ToTable(\"PostTags\")"), "scaffolded context is wrong");
});

Step("insert", () =>
{
    using var db = Db();
    var news = new Tag { Name = "news" };
    var rust = new Tag { Name = "rust" };
    var sql = new Tag { Name = "sql" };
    db.Tags.AddRange(news, rust, sql);
    db.Users.Add(new User
    {
        Email = "alice@example.com", Name = "Alice", Balance = 100.50m,
        Profile = """{"city": "Tokyo", "tags": ["a", "b"]}""",
        Posts =
        {
            new Post { Title = "Hello", Body = "First post", PublishedAt = new DateTime(2024, 1, 2, 3, 4, 5), Views = 10, Tags = { news, rust } },
            new Post { Title = "Draft", Body = "Not yet", Views = 0 },
        },
    });
    db.Users.Add(new User
    {
        Email = "bob@example.com", Name = "Bob", Balance = 20.25m, IsActive = false,
        Profile = """{"city": "Osaka", "tags": ["c"]}""",
        Posts = { new Post { Title = "Bob writes", Views = 3, Tags = { sql } } },
    });
    db.Users.Add(new User { Email = "carol@example.com", Name = "Carol", Balance = 5m });
    db.SaveChanges();
    Expect(db.Users.All(u => u.Id > 0) && db.Posts.Count() == 3, "insert counts");
    Expect(db.Database.SqlQuery<int>($"SELECT COUNT(*) AS Value FROM PostTags").Single() == 3, "PostTags rows");
    var alice = db.Users.Single(u => u.Email == "alice@example.com");
    Expect(alice.UpdatedAt > DateTime.MinValue && alice.CreatedAt > DateTime.MinValue, "store-generated timestamps were not read back");
});

Step("relations", () =>
{
    using var db = Db();
    var users = db.Users.Include(u => u.Posts.OrderBy(p => p.Id)).ThenInclude(p => p.Tags).OrderBy(u => u.Id).ToList();
    Expect(users.Count == 3, $"expected 3 users, got {users.Count}");
    Expect(string.Join(",", users[0].Posts[0].Tags.Select(t => t.Name).Order()) == "news,rust", "tags of the first post");
    var split = db.Users.Include(u => u.Posts).ThenInclude(p => p.Tags).AsSplitQuery().OrderBy(u => u.Id).ToList();
    Expect(split[1].Posts.Single().Tags.Single().Name == "sql", "split query");
    var tagged = db.Posts.Where(p => p.Tags.Any(t => t.Name == "rust") && p.User.IsActive).Select(p => p.User.Email).ToList();
    Expect(tagged.Count == 1 && tagged[0] == "alice@example.com", "Any() over the join table");
    var lonely = db.Users.Where(u => !u.Posts.Any()).Select(u => u.Name).ToList();
    Expect(lonely.Count == 1 && lonely[0] == "Carol", "anti-join");
    var counts = db.Users.OrderBy(u => u.Id).Select(u => new { u.Name, Posts = u.Posts.Count(), Views = u.Posts.Sum(p => (int?)p.Views) ?? 0 }).ToList();
    Expect(counts[0].Posts == 2 && counts[0].Views == 10 && counts[2].Posts == 0, "correlated counts");
});

Step("update", () =>
{
    using var db = Db();
    var alice = db.Users.Single(u => u.Email == "alice@example.com");
    var before = alice.UpdatedAt;
    Thread.Sleep(20);
    alice.Balance += 9.50m;
    alice.Name = "Alice A.";
    db.SaveChanges();
    Expect(alice.UpdatedAt > before, $"row version did not move: {before:O} -> {alice.UpdatedAt:O}");
    var bumped = db.Posts.Where(p => p.Views < 5).ExecuteUpdate(s => s.SetProperty(p => p.Views, p => p.Views + 1));
    Expect(bumped == 2, $"ExecuteUpdate touched {bumped}");
    using var fresh = Db();
    var reread = fresh.Users.AsNoTracking().Single(u => u.Email == "alice@example.com");
    Expect(reread.Balance == 110m && reread.Name == "Alice A.", $"alice is {reread.Name} with {reread.Balance}");
});

Step("concurrency", () =>
{
    using var first = Db();
    using var second = Db();
    var a = first.Users.Single(u => u.Email == "bob@example.com");
    var b = second.Users.Single(u => u.Email == "bob@example.com");
    Thread.Sleep(20);
    a.Name = "Bob One";
    first.SaveChanges();
    b.Name = "Bob Two";
    try
    {
        second.SaveChanges();
        throw new Exception("saving a stale row version succeeded");
    }
    catch (DbUpdateConcurrencyException e)
    {
        Console.WriteLine($"stale save refused: {e.Message}");
    }
    using var check = Db();
    Expect(check.Users.Single(u => u.Email == "bob@example.com").Name == "Bob One", "bob was overwritten");
});

Step("delete", () =>
{
    using var db = Db();
    var dave = new User { Email = "dave@example.com", Name = "Dave", Posts = { new Post { Title = "Bye" } } };
    db.Users.Add(dave);
    db.SaveChanges();
    using (var other = Db())
    {
        other.Users.Remove(other.Users.Single(u => u.Id == dave.Id));
        other.SaveChanges();
    }
    Expect(db.Posts.Count(p => p.UserId == dave.Id) == 0, "ON DELETE CASCADE left posts behind");
    db.Tags.Add(new Tag { Name = "temp" });
    db.SaveChanges();
    Expect(db.Tags.Where(t => t.Name.StartsWith("te")).ExecuteDelete() == 1, "ExecuteDelete");
});

Step("pagination", () =>
{
    using var db = Db();
    var page = db.Posts.OrderBy(p => p.Id).Skip(1).Take(2).ToList();
    Expect(page.Count == 2 && db.Posts.Count() == 3, $"page {page.Count}");
    var withTags = db.Posts.Include(p => p.Tags).Include(p => p.User).OrderBy(p => p.Id).Skip(0).Take(1).ToList();
    Expect(withTags.Count == 1 && withTags[0].Tags.Count == 2 && withTags[0].User.Name == "Alice A.", "paged include");
    var names = db.Users.OrderByDescending(u => u.Balance).Select(u => u.Name).Skip(1).Take(1).Single();
    Expect(names == "Bob One", $"second richest is {names}");
});

Step("aggregate", () =>
{
    using var db = Db();
    var groups = db.Users.GroupBy(u => u.IsActive).Where(g => g.Count() > 0)
        .Select(g => new { Active = g.Key, Count = g.Count(), Total = g.Sum(u => u.Balance), Average = g.Average(u => u.Balance), Latest = g.Max(u => u.CreatedAt) })
        .OrderBy(g => g.Active).ToList();
    Console.WriteLine(JsonSerializer.Serialize(groups));
    Expect(groups.Count == 2 && groups[1].Count == 2 && groups[1].Total == 115m, "groups");
    var busy = db.Posts.GroupBy(p => p.UserId).Where(g => g.Sum(p => p.Views) > 5).Select(g => g.Key).ToList();
    Expect(busy.Count == 1, $"HAVING SUM returned {busy.Count}");
    Expect(db.Posts.Max(p => p.Views) == 10 && db.Users.Count(u => u.IsActive) == 2 && db.Posts.Select(p => p.UserId).Distinct().Count() == 2, "scalar aggregates");
});

Step("transaction-commit", () =>
{
    using var db = Db();
    using (var tx = db.Database.BeginTransaction())
    {
        db.Users.Where(u => u.Email == "alice@example.com").ExecuteUpdate(s => s.SetProperty(u => u.Balance, u => u.Balance - 10));
        db.Users.Where(u => u.Email == "bob@example.com").ExecuteUpdate(s => s.SetProperty(u => u.Balance, u => u.Balance + 10));
        var bob = db.Users.Single(u => u.Email == "bob@example.com");
        db.Posts.Add(new Post { Title = "In a transaction", UserId = bob.Id });
        db.SaveChanges();
        tx.Commit();
    }
    using var check = Db();
    Expect(check.Users.Single(u => u.Email == "bob@example.com").Balance == 30.25m, "bob's balance");
    Expect(check.Posts.Count() == 4, "transaction did not commit");
});

Step("transaction-rollback", () =>
{
    using (var db = Db())
    {
        using var tx = db.Database.BeginTransaction();
        db.Users.Where(u => u.Email == "alice@example.com").ExecuteUpdate(s => s.SetProperty(u => u.Balance, 0m));
        db.Posts.ExecuteDelete();
        tx.Rollback();
    }
    using var check = Db();
    Expect(check.Users.Single(u => u.Email == "alice@example.com").Balance == 100m, "alice after rollback");
    Expect(check.Posts.Count() == 4, "rolled back deletes are visible");
});

Step("savepoint", () =>
{
    using var db = Db();
    using (var tx = db.Database.BeginTransaction())
    {
        db.Tags.Add(new Tag { Name = "kept" });
        db.SaveChanges();
        tx.CreateSavepoint("before_duplicate");
        db.Tags.Add(new Tag { Name = "dropped" });
        db.Tags.Add(new Tag { Name = "kept" });
        try
        {
            db.SaveChanges();
            throw new Exception("the duplicate insert succeeded");
        }
        catch (DbUpdateException e)
        {
            Console.WriteLine($"duplicate refused: {e.InnerException?.Message}");
        }
        tx.RollbackToSavepoint("before_duplicate");
        db.ChangeTracker.Clear();
        db.Tags.Add(new Tag { Name = "after" });
        db.SaveChanges();
        tx.Commit();
    }
    var names = string.Join(",", db.Tags.Where(t => new[] { "kept", "dropped", "after" }.Contains(t.Name)).OrderBy(t => t.Name).Select(t => t.Name));
    Expect(names == "after,kept", $"tags after the savepoint rollback: {names}");
});

Step("isolation", () =>
{
    using var db = Db();
    using (var tx = db.Database.BeginTransaction(IsolationLevel.ReadCommitted))
    {
        Expect(db.Users.Count() == 3, "count under READ COMMITTED");
        tx.Commit();
    }
    using (var tx = db.Database.BeginTransaction(IsolationLevel.Serializable))
    {
        var carol = db.Users.Single(u => u.Email == "carol@example.com");
        carol.Balance += 1;
        db.SaveChanges();
        tx.Commit();
    }
    Expect(db.Users.AsNoTracking().Single(u => u.Email == "carol@example.com").Balance == 6m, "carol under SERIALIZABLE");
});

Step("json", () =>
{
    using var db = Db();
    var city = "Tokyo";
    var tokyo = db.Users.FromSql($"SELECT * FROM Users WHERE JSON_UNQUOTE(JSON_EXTRACT(Profile, '$.city')) = {city}").ToList();
    Expect(tokyo.Count == 1 && tokyo[0].Email == "alice@example.com", "JSON_EXTRACT through FromSql");
    var osaka = db.Database.SqlQuery<int>($"SELECT COUNT(*) AS Value FROM Users WHERE Profile->>'$.city' = {"Osaka"}").Single();
    Expect(osaka == 1, "->> through SqlQuery");
    var moved = db.Database.ExecuteSql($"UPDATE Users SET Profile = JSON_SET(Profile, '$.city', {"Kyoto"}) WHERE Email = {"bob@example.com"}");
    Expect(moved == 1, $"JSON_SET touched {moved}");
    var bob = db.Users.AsNoTracking().Single(u => u.Email == "bob@example.com");
    using var doc = JsonDocument.Parse(bob.Profile!);
    Expect(doc.RootElement.GetProperty("city").GetString() == "Kyoto" && doc.RootElement.GetProperty("tags")[0].GetString() == "c", $"profile {bob.Profile}");
    var tagged = db.Users.Count(u => EF.Functions.Like(u.Profile!, "%\"a\"%"));
    Expect(tagged == 1, $"LIKE over a JSON column matched {tagged}");
});

Step("upsert", () =>
{
    using var db = Db();
    db.Database.ExecuteSql($"INSERT INTO Tags (Name) VALUES ({"go"}), ({"news"}) ON DUPLICATE KEY UPDATE Name = VALUES(Name)");
    Expect(db.Tags.Count(t => t.Name == "go") == 1 && db.Tags.Count(t => t.Name == "news") == 1, "tags upsert");
    db.Database.ExecuteSql(
        $"INSERT INTO Users (Email, Name, Balance) VALUES ({"alice@example.com"}, {"Alice Upserted"}, {100.00m}), ({"erin@example.com"}, {"Erin"}, {0m}) AS new ON DUPLICATE KEY UPDATE Name = new.Name, Balance = new.Balance");
    Expect(db.Users.Single(u => u.Email == "alice@example.com").Name == "Alice Upserted", "upsert did not update");
    Expect(db.Users.Count() == 4, "upsert did not insert");
});

Step("alter-migration", () =>
{
    Ef("database", "update", "AddSlugAndRenameViews");
    using var db = Db();
    var columns = ColumnsOf(db, "Posts");
    Expect(columns.ContainsKey("Slug") && columns.ContainsKey("ViewCount") && !columns.ContainsKey("Views"), $"Posts columns {string.Join(",", columns.Keys)}");
    Expect(columns["Title"] == "varchar(255)", $"Posts.Title is {columns["Title"]}");
    db.Database.ExecuteSql($"UPDATE Posts SET Slug = CONCAT('post-', Id)");
    Expect(db.Database.SqlQuery<int>($"SELECT CAST(SUM(ViewCount) AS SIGNED) AS Value FROM Posts").Single() == 15, "ViewCount sum");
});

Step("rollback-migration", () =>
{
    Ef("database", "update", "CreateBlog");
    using var db = Db();
    var columns = ColumnsOf(db, "Posts");
    Expect(!columns.ContainsKey("Slug") && columns.ContainsKey("Views") && columns["Title"] == "varchar(200)", $"Posts columns {string.Join(",", columns.Keys)}");
    Expect(db.Posts.Sum(p => p.Views) == 15, "the model reads the rolled back table");
    Expect(db.Database.GetPendingMigrations().Single().EndsWith("_AddSlugAndRenameViews"), "the rolled back migration is not pending");
});

Step("rollback-all", () =>
{
    Ef("database", "update", "0");
    using var db = Db();
    var tables = TableNames(db);
    Expect(tables.SequenceEqual(new[] { "__EFMigrationsHistory" }), $"tables left: {string.Join(",", tables)}");
});

Step("ensure-created", () =>
{
    using (var db = Db())
    {
        Expect(db.Database.EnsureDeleted(), "EnsureDeleted did not drop the database");
        Expect(db.Database.EnsureCreated(), "EnsureCreated did not create the database");
        db.Users.Add(new User { Email = "x@example.com", Name = "X", Posts = { new Post { Title = "x", Tags = { new Tag { Name = "x" } } } } });
        db.SaveChanges();
    }
    using var check = Db();
    Expect(check.Posts.Include(p => p.Tags).Single().Tags.Single().Name == "x", "the created schema does not round-trip");
});

static List<string> TableNames(BlogContext db) =>
    db.Database.SqlQuery<string>($"SELECT TABLE_NAME AS Value FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME").ToList();

static Dictionary<string, string> ColumnsOf(BlogContext db, string table) =>
    db.Database.SqlQuery<ColumnRow>($"SELECT COLUMN_NAME AS Name, COLUMN_TYPE AS Type FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = {table}")
        .ToList().ToDictionary(c => c.Name, c => c.Type);

// Runs `dotnet ef` on the already built project, as a developer would on a checkout.
static string Ef(params string[] args)
{
    var start = new ProcessStartInfo("dotnet") { RedirectStandardOutput = true, RedirectStandardError = true, WorkingDirectory = "/app" };
    foreach (var a in new[] { "ef" }.Concat(args).Concat(new[] { "--no-build", "--prefix-output" })) start.ArgumentList.Add(a);
    Console.WriteLine($"$ dotnet ef {string.Join(" ", args.Select(a => a.Contains("Password=") ? "<connection string>" : a))}");
    using var process = Process.Start(start)!;
    var stdout = process.StandardOutput.ReadToEndAsync();
    var stderr = process.StandardError.ReadToEndAsync();
    process.WaitForExit();
    var output = stdout.Result + stderr.Result;
    Console.WriteLine(output);
    if (process.ExitCode != 0) throw new Exception($"dotnet ef {args[0]} {args[1]} exited with {process.ExitCode}:\n{output}");
    return output;
}

static void Step(string name, Action body)
{
    Console.WriteLine($"=== step {name}");
    Dictionary<string, object> entry;
    try
    {
        body();
        entry = new() { ["step"] = name, ["ok"] = true };
    }
    catch (Exception e)
    {
        var text = e.ToString();
        Console.WriteLine(text);
        entry = new() { ["step"] = name, ["ok"] = false, ["error"] = text.Length > 2000 ? text[^2000..] : text };
    }
    File.AppendAllText(Path.Combine(Environment.GetEnvironmentVariable("E2E_OUT")!, "steps.jsonl"), JsonSerializer.Serialize(entry) + "\n");
}

static void Expect(bool condition, string message)
{
    if (!condition) throw new Exception(message);
}

record ColumnRow(string Name, string Type);
