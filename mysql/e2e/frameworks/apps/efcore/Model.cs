using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Design;
using Microsoft.EntityFrameworkCore.Diagnostics;

namespace E2e;

// The blog schema every harness app uses. The app runs against the first
// release; SCHEMA_V2 is the next release, compiled only to generate the
// AddSlugAndRenameViews migration, which the alter-migration step applies.
public class User
{
    public long Id { get; set; }
    public string Email { get; set; } = "";
    public string Name { get; set; } = "";
    public decimal Balance { get; set; }
    public bool IsActive { get; set; } = true;
    public string? Profile { get; set; }
    public DateTime CreatedAt { get; set; }
    public DateTime UpdatedAt { get; set; }
    public List<Post> Posts { get; set; } = new();
}

public class Post
{
    public long Id { get; set; }
    public long UserId { get; set; }
    public User User { get; set; } = null!;
    public string Title { get; set; } = "";
#if SCHEMA_V2
    public string? Slug { get; set; }
#endif
    public string? Body { get; set; }
    public DateTime? PublishedAt { get; set; }
#if SCHEMA_V2
    public int ViewCount { get; set; }
#else
    public int Views { get; set; }
#endif
    public List<Tag> Tags { get; set; } = new();
}

public class Tag
{
    public long Id { get; set; }
    public string Name { get; set; } = "";
    public List<Post> Posts { get; set; } = new();
}

public class BlogContext(DbContextOptions<BlogContext> options) : DbContext(options)
{
    public DbSet<User> Users => Set<User>();
    public DbSet<Post> Posts => Set<Post>();
    public DbSet<Tag> Tags => Set<Tag>();

    protected override void OnModelCreating(ModelBuilder model)
    {
        model.Entity<User>(e =>
        {
            e.Property(u => u.Email).HasMaxLength(191);
            e.HasIndex(u => u.Email).IsUnique();
            e.Property(u => u.Name).HasMaxLength(100);
            e.Property(u => u.Balance).HasPrecision(10, 2).HasDefaultValue(0m);
            e.Property(u => u.IsActive).HasDefaultValue(true);
            e.Property(u => u.Profile).HasColumnType("json");
            e.Property(u => u.CreatedAt).HasColumnType("datetime(6)").HasDefaultValueSql("CURRENT_TIMESTAMP(6)");
            // Pomelo maps a DateTime row version to TIMESTAMP(6) ... ON UPDATE CURRENT_TIMESTAMP(6).
            e.Property(u => u.UpdatedAt).IsRowVersion();
        });
        model.Entity<Post>(e =>
        {
#if SCHEMA_V2
            e.Property(p => p.Title).HasMaxLength(255);
            e.Property(p => p.Slug).HasMaxLength(220);
            e.HasIndex(p => p.Slug).IsUnique();
#else
            e.Property(p => p.Title).HasMaxLength(200);
#endif
            e.Property(p => p.Body).HasColumnType("text");
            e.Property(p => p.PublishedAt).HasColumnType("datetime(6)");
            e.HasOne(p => p.User).WithMany(u => u.Posts).HasForeignKey(p => p.UserId).OnDelete(DeleteBehavior.Cascade);
            e.HasIndex(p => new { p.UserId, p.PublishedAt });
            e.HasMany(p => p.Tags).WithMany(t => t.Posts).UsingEntity("PostTags");
        });
        model.Entity<Tag>(e =>
        {
            e.Property(t => t.Name).HasMaxLength(100);
            e.HasIndex(t => t.Name).IsUnique();
        });
    }

    public static DbContextOptions<BlogContext> Options(ServerVersion? version = null)
    {
        var cs = ConnectionString();
        return new DbContextOptionsBuilder<BlogContext>()
            .UseMySql(cs, version ?? ServerVersion.AutoDetect(cs))
            // The model is the first release while the migrations also hold the next one.
            .ConfigureWarnings(w => w.Ignore(RelationalEventId.PendingModelChangesWarning))
            .LogTo(Console.WriteLine, new[] { DbLoggerCategory.Database.Command.Name }, Microsoft.Extensions.Logging.LogLevel.Information)
            .Options;
    }

    public static string ConnectionString() =>
        $"Server={Env("E2E_HOST", "127.0.0.1")};Port={Env("E2E_PORT", "3306")};Database={Env("E2E_APP", "efcore")};" +
        $"User ID={Env("E2E_USER", "e2e")};Password={Env("E2E_PASSWORD", "")};" +
        $"SslMode=VerifyFull;SslCa={Env("E2E_CA", "/e2e/tls/ca.pem")}";

    static string Env(string name, string fallback) => Environment.GetEnvironmentVariable(name) ?? fallback;
}

// Used by `dotnet ef`. Generating migrations needs no server, so it may name the version.
public class DesignTimeFactory : IDesignTimeDbContextFactory<BlogContext>
{
    public BlogContext CreateDbContext(string[] args)
    {
        var fixedVersion = Environment.GetEnvironmentVariable("E2E_SERVER_VERSION");
        return new BlogContext(BlogContext.Options(fixedVersion is null ? null : ServerVersion.Parse(fixedVersion)));
    }
}
