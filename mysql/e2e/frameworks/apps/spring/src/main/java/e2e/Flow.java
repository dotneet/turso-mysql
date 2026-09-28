package e2e;

import e2e.Blog.ActiveGroup;
import e2e.Blog.Post;
import e2e.Blog.PostRepository;
import e2e.Blog.Tag;
import e2e.Blog.TagRepository;
import e2e.Blog.User;
import e2e.Blog.UserRepository;
import jakarta.persistence.EntityManagerFactory;
import java.math.BigDecimal;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.time.LocalDateTime;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import javax.sql.DataSource;
import org.flywaydb.core.Flyway;
import org.flywaydb.core.api.output.MigrateResult;
import org.hibernate.SessionFactory;
import org.hibernate.StatelessSession;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.dao.DataIntegrityViolationException;
import org.springframework.dao.DuplicateKeyException;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.PageRequest;
import org.springframework.data.domain.Slice;
import org.springframework.data.domain.Sort;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.jdbc.datasource.DataSourceTransactionManager;
import org.springframework.orm.ObjectOptimisticLockingFailureException;
import org.springframework.transaction.PlatformTransactionManager;
import org.springframework.transaction.TransactionDefinition;
import org.springframework.transaction.support.TransactionTemplate;

/** The steps, in order. Everything after start-context needs the Spring context. */
final class Flow {
  private final Main.Steps steps;
  private final String url;
  private final String user;
  private final String password;
  private ConfigurableApplicationContext ctx;
  private UserRepository users;
  private PostRepository posts;
  private TagRepository tags;
  private JdbcTemplate jdbc;
  private TransactionTemplate tx;

  Flow(Main.Steps steps, String url, String user, String password) {
    this.steps = steps;
    this.url = url;
    this.user = user;
    this.password = password;
  }

  void run() {
    steps.run("connect", this::connect);
    steps.run("migrate", () -> {
      MigrateResult result = flyway("1").migrate();
      expect(result.migrationsExecuted == 1, "migrated " + result.migrationsExecuted);
      expect(tableNames().containsAll(List.of("flyway_schema_history", "users", "posts", "tags", "post_tags")), "tables " + tableNames());
    });
    steps.run("migrate-again", () -> {
      Flyway flyway = flyway("1");
      expect(flyway.migrate().migrationsExecuted == 0, "a second migrate ran migrations");
      flyway.validate();
      expect(flyway.info().applied().length == 1, "applied " + flyway.info().applied().length);
    });
    steps.run("start-context", () -> start());
    steps.run("insert", this::insert);
    steps.run("relations", this::relations);
    steps.run("update", this::update);
    steps.run("optimistic-lock", this::optimisticLock);
    steps.run("pessimistic-lock", this::pessimisticLock);
    steps.run("delete", this::delete);
    steps.run("pagination", this::pagination);
    steps.run("aggregate", this::aggregate);
    steps.run("transaction-commit", this::transactionCommit);
    steps.run("transaction-rollback", this::transactionRollback);
    steps.run("savepoint", this::savepoint);
    steps.run("isolation-read-only", this::isolationReadOnly);
    steps.run("json", this::json);
    steps.run("upsert", this::upsert);
    steps.run("batch", this::batch);
    steps.run("metadata", this::metadata);
    steps.run("alter-migration", this::alterMigration);
    steps.run("revert-migration", this::revertMigration);
    steps.run("flyway-clean", this::flywayClean);
    if (ctx != null) ctx.close();
  }

  private void connect() throws Exception {
    try (Connection c = DriverManager.getConnection(url, user, password);
        ResultSet rs = c.createStatement().executeQuery("SELECT DATABASE(), @@version, @@transaction_isolation")) {
      rs.next();
      System.out.println(c.getMetaData().getDatabaseProductName() + " " + c.getMetaData().getDatabaseProductVersion()
          + " driver " + c.getMetaData().getDriverVersion() + ": " + rs.getString(1) + " " + rs.getString(2) + " " + rs.getString(3));
      expect(c.getCatalog().equals(rs.getString(1)), "DATABASE() is " + rs.getString(1));
      expect(c.getMetaData().getDatabaseMajorVersion() == 8, "major version " + c.getMetaData().getDatabaseMajorVersion());
    }
  }

  private Flyway flyway(String target) {
    return Flyway.configure().dataSource(url, user, password).target(target).cleanDisabled(false).load();
  }

  private void start() {
    Map<String, Object> props = new HashMap<>();
    props.put("spring.datasource.url", url);
    props.put("spring.datasource.username", user);
    props.put("spring.datasource.password", password);
    ctx = new SpringApplicationBuilder(Main.class).properties(props).run();
    users = ctx.getBean(UserRepository.class);
    posts = ctx.getBean(PostRepository.class);
    tags = ctx.getBean(TagRepository.class);
    jdbc = new JdbcTemplate(ctx.getBean(DataSource.class));
    tx = new TransactionTemplate(ctx.getBean(PlatformTransactionManager.class));
  }

  private void insert() {
    requireContext();
    List<Tag> saved = tags.saveAll(List.of(new Tag("news"), new Tag("rust"), new Tag("sql")));
    expect(saved.stream().allMatch(t -> t.id != null), "tags have no ids");
    tx.executeWithoutResult(s -> {
      User alice = users.save(new User("alice@example.com", "Alice", "100.50", true, Map.of("city", "Tokyo", "tags", List.of("a", "b"))));
      User bob = users.save(new User("bob@example.com", "Bob", "20.25", false, Map.of("city", "Osaka", "tags", List.of("c"))));
      users.save(new User("carol@example.com", "Carol", "5.00", true, null));
      Post hello = new Post(alice, "Hello", "First post", LocalDateTime.of(2024, 1, 2, 3, 4, 5), 10);
      hello.tags.add(tags.findByName("news").orElseThrow());
      hello.tags.add(tags.findByName("rust").orElseThrow());
      Post bobs = new Post(bob, "Bob writes", null, null, 3);
      bobs.tags.add(tags.findByName("sql").orElseThrow());
      posts.saveAll(List.of(hello, new Post(alice, "Draft", "Not yet", null, 0), bobs));
    });
    expect(posts.count() == 3, "expected 3 posts");
    expect(jdbc.queryForObject("SELECT COUNT(*) FROM post_tags", Long.class) == 3, "expected 3 post_tags rows");
  }

  private void relations() {
    requireContext();
    tx.executeWithoutResult(s -> {
      List<Post> all = posts.findAllWithUserAndTags();
      expect(all.size() == 3, "expected 3 posts, got " + all.size());
      Set<String> names = new TreeSet<>();
      all.get(0).tags.forEach(t -> names.add(t.name));
      expect(names.toString().equals("[news, rust]"), "tags of the first post: " + names);
      expect(all.get(0).user.email.equals("alice@example.com"), "author of the first post");
      List<Post> rust = posts.findActiveByTag("rust");
      expect(rust.size() == 1 && rust.get(0).user.name.equals("Alice"), "join fetch by tag returned " + rust.size());
      expect(posts.findByTagsName("sql").size() == 1, "derived query over the join table");
      expect(posts.countByUserEmail("alice@example.com") == 2, "derived count over a join");
      List<User> lonely = users.findWithoutPosts();
      expect(lonely.size() == 1 && lonely.get(0).name.equals("Carol"), "NOT EXISTS returned " + lonely.size());
      User alice = users.findByEmail("alice@example.com").orElseThrow();
      expect(alice.posts.size() == 2, "lazy collection has " + alice.posts.size());
    });
  }

  private void update() throws Exception {
    requireContext();
    User before = users.findByEmail("alice@example.com").orElseThrow();
    Thread.sleep(20);
    tx.executeWithoutResult(s -> {
      User alice = users.findByEmail("alice@example.com").orElseThrow();
      alice.balance = alice.balance.add(new BigDecimal("9.50"));
      alice.name = "Alice A.";
    });
    User after = users.findByEmail("alice@example.com").orElseThrow();
    expect(after.balance.compareTo(new BigDecimal("110.00")) == 0 && after.name.equals("Alice A."), "alice is " + after.name + " with " + after.balance);
    expect(after.version == before.version + 1, "version went from " + before.version + " to " + after.version);
    expect(after.updatedAt.isAfter(before.updatedAt), "updated_at did not move");
    Integer bumped = tx.execute(s -> posts.bumpViewsBelow(5));
    expect(bumped == 2, "bulk JPQL update touched " + bumped);
  }

  private void optimisticLock() {
    requireContext();
    User first = users.findByEmail("bob@example.com").orElseThrow();
    User second = users.findByEmail("bob@example.com").orElseThrow();
    first.name = "Bob One";
    users.save(first);
    second.name = "Bob Two";
    try {
      users.save(second);
      throw new AssertionError("saving a stale copy succeeded");
    } catch (ObjectOptimisticLockingFailureException expected) {
      System.out.println("stale save refused: " + expected.getMessage());
    }
    expect(users.findByEmail("bob@example.com").orElseThrow().name.equals("Bob One"), "bob was overwritten");
  }

  private void pessimisticLock() {
    requireContext();
    tx.executeWithoutResult(s -> {
      User carol = users.lockByEmail("carol@example.com").orElseThrow();
      carol.balance = carol.balance.add(BigDecimal.ONE);
      expect(posts.readShared().size() == 3, "FOR SHARE read the wrong posts");
    });
    expect(users.findByEmail("carol@example.com").orElseThrow().balance.compareTo(new BigDecimal("6.00")) == 0, "carol's balance");
  }

  private void delete() {
    requireContext();
    User dave = users.save(new User("dave@example.com", "Dave", "0", true, null));
    posts.save(new Post(dave, "Bye", null, null, 0));
    users.delete(users.findById(dave.id).orElseThrow());
    expect(jdbc.queryForObject("SELECT COUNT(*) FROM posts WHERE user_id = ?", Long.class, dave.id) == 0, "ON DELETE CASCADE left posts behind");
    Tag temp = tags.save(new Tag("temp"));
    tags.deleteById(temp.id);
    expect(tags.findByName("temp").isEmpty(), "deleteById did not delete the tag");
  }

  private void pagination() {
    requireContext();
    Page<Post> page = posts.findAll(PageRequest.of(1, 2, Sort.by("id")));
    expect(page.getNumberOfElements() == 1 && page.getTotalElements() == 3 && page.getTotalPages() == 2,
        "page " + page.getNumberOfElements() + " of " + page.getTotalElements());
    Page<Post> withUser = posts.pageWithUser(PageRequest.of(0, 2, Sort.by(Sort.Direction.DESC, "id")));
    expect(withUser.getContent().size() == 2 && withUser.getTotalElements() == 3, "join fetch page");
    Slice<Post> slice = posts.findByViewsGreaterThanEqual(1, PageRequest.of(0, 1, Sort.by("views")));
    expect(slice.hasNext() && slice.getContent().get(0).views == 1, "slice");
  }

  private void aggregate() {
    requireContext();
    List<ActiveGroup> groups = users.groupByActive(0);
    System.out.println(groups);
    expect(groups.size() == 2 && groups.get(1).count() == 2 && groups.get(1).total().compareTo(new BigDecimal("116.00")) == 0,
        "groups " + groups);
    List<Object[]> busy = posts.busyAuthors(5);
    expect(busy.size() == 1, "HAVING SUM returned " + busy.size());
    Map<String, Object> row = jdbc.queryForMap(
        "SELECT COUNT(DISTINCT p.user_id) AS authors, MAX(p.views) AS top, GROUP_CONCAT(t.name ORDER BY t.name SEPARATOR ',') AS names"
            + " FROM posts p LEFT JOIN post_tags pt ON pt.post_id = p.id LEFT JOIN tags t ON t.id = pt.tag_id");
    expect(((Number) row.get("authors")).intValue() == 2 && "news,rust,sql".equals(row.get("names")), "aggregate row " + row);
  }

  private void transactionCommit() {
    requireContext();
    tx.executeWithoutResult(s -> {
      User alice = users.findByEmail("alice@example.com").orElseThrow();
      User bob = users.findByEmail("bob@example.com").orElseThrow();
      alice.balance = alice.balance.subtract(BigDecimal.TEN);
      bob.balance = bob.balance.add(BigDecimal.TEN);
      posts.save(new Post(bob, "In a transaction", null, null, 0));
    });
    expect(users.findByEmail("bob@example.com").orElseThrow().balance.compareTo(new BigDecimal("30.25")) == 0, "bob's balance");
    expect(posts.count() == 4, "transaction did not commit");
  }

  private void transactionRollback() {
    requireContext();
    try {
      tx.executeWithoutResult(s -> {
        users.findByEmail("alice@example.com").orElseThrow().balance = BigDecimal.ZERO;
        users.flush();
        posts.deleteAllInBatch();
        throw new IllegalStateException("roll back on purpose");
      });
    } catch (IllegalStateException expected) {
      System.out.println(expected.getMessage());
    }
    tx.executeWithoutResult(s -> {
      tags.findByName("news").orElseThrow().name = "breaking";
      s.setRollbackOnly();
    });
    expect(users.findByEmail("alice@example.com").orElseThrow().balance.compareTo(new BigDecimal("100.00")) == 0, "alice after rollback");
    expect(posts.count() == 4, "rolled back deletes are visible");
    expect(tags.findByName("news").isPresent(), "rolled back rename is visible");
  }

  /** PROPAGATION_NESTED on plain JDBC: Connector/J's setSavepoint / rollback(savepoint). */
  private void savepoint() {
    requireContext();
    DataSourceTransactionManager manager = new DataSourceTransactionManager(ctx.getBean(DataSource.class));
    TransactionTemplate outer = new TransactionTemplate(manager);
    TransactionTemplate nested = new TransactionTemplate(manager);
    nested.setPropagationBehavior(TransactionDefinition.PROPAGATION_NESTED);
    outer.executeWithoutResult(s -> {
      jdbc.update("INSERT INTO tags (name) VALUES ('kept')");
      try {
        nested.executeWithoutResult(n -> {
          jdbc.update("INSERT INTO tags (name) VALUES ('dropped')");
          jdbc.update("INSERT INTO tags (name) VALUES ('kept')");
        });
        throw new AssertionError("the duplicate insert succeeded");
      } catch (DuplicateKeyException expected) {
        System.out.println("rolled back to the savepoint: " + expected.getMessage());
      }
      jdbc.update("INSERT INTO tags (name) VALUES ('after')");
    });
    List<String> names = jdbc.queryForList("SELECT name FROM tags WHERE name IN ('kept', 'dropped', 'after') ORDER BY name", String.class);
    expect(names.equals(List.of("after", "kept")), "tags after the savepoint rollback: " + names);
  }

  private void isolationReadOnly() {
    requireContext();
    TransactionTemplate committed = new TransactionTemplate(ctx.getBean(PlatformTransactionManager.class));
    committed.setIsolationLevel(TransactionDefinition.ISOLATION_READ_COMMITTED);
    String level = committed.execute(s -> jdbc.queryForObject("SELECT @@transaction_isolation", String.class));
    expect("READ-COMMITTED".equals(level), "isolation inside the transaction is " + level);
    TransactionTemplate readOnly = new TransactionTemplate(ctx.getBean(PlatformTransactionManager.class));
    readOnly.setReadOnly(true);
    Long count = readOnly.execute(s -> users.count());
    expect(count == 3, "read-only count " + count);
    try {
      readOnly.executeWithoutResult(s -> jdbc.update("UPDATE users SET name = name"));
      throw new AssertionError("an UPDATE inside a read-only transaction succeeded");
    } catch (org.springframework.dao.DataAccessException expected) {
      System.out.println("write refused in a read-only transaction: " + expected.getMessage());
    }
  }

  private void json() {
    requireContext();
    List<User> tokyo = users.findByCity("Tokyo");
    expect(tokyo.size() == 1 && tokyo.get(0).email.equals("alice@example.com"), "JSON_EXTRACT native query");
    expect(users.countByCityArrow("Osaka") == 1, "->> native query");
    User bob = users.findByEmail("bob@example.com").orElseThrow();
    expect("Osaka".equals(bob.profile.get("city")) && List.of("c").equals(bob.profile.get("tags")), "json round trip " + bob.profile);
    int moved = tx.execute(s -> users.moveTo("bob@example.com", "Kyoto"));
    expect(moved == 1, "JSON_SET touched " + moved);
    expect("Kyoto".equals(users.findByEmail("bob@example.com").orElseThrow().profile.get("city")), "JSON_SET did not stick");
    tx.executeWithoutResult(s -> {
      User carol = users.findByEmail("carol@example.com").orElseThrow();
      carol.profile = new HashMap<>(Map.of("city", "Nagoya", "tags", List.of()));
    });
    expect(users.findByCity("Nagoya").size() == 1, "profile written by Hibernate is not queryable");
  }

  private void upsert() {
    requireContext();
    tx.executeWithoutResult(s -> {
      users.upsert("alice@example.com", "Alice Upserted", new BigDecimal("100.00"));
      users.upsert("erin@example.com", "Erin", BigDecimal.ZERO);
    });
    expect(users.findByEmail("alice@example.com").orElseThrow().name.equals("Alice Upserted"), "upsert did not update");
    expect(users.count() == 4, "upsert did not insert");
    Tag sql = tags.findByName("sql").orElseThrow();
    try (StatelessSession session = ctx.getBean(EntityManagerFactory.class).unwrap(SessionFactory.class).openStatelessSession()) {
      session.getTransaction().begin();
      Tag renamed = new Tag("sql2");
      renamed.id = sql.id;
      session.upsert(renamed);
      session.getTransaction().commit();
    }
    expect(tags.findById(sql.id).orElseThrow().name.equals("sql2"), "StatelessSession.upsert did not update");
    try {
      tags.saveAndFlush(new Tag("rust"));
      throw new AssertionError("a duplicate tag was saved");
    } catch (DataIntegrityViolationException expected) {
      System.out.println("duplicate refused: " + expected.getMostSpecificCause().getMessage());
    }
  }

  /** rewriteBatchedStatements: INSERT batches become one multi-row INSERT, UPDATE batches one multi-statement. */
  private void batch() {
    requireContext();
    List<Object[]> rows = new ArrayList<>();
    for (int i = 0; i < 50; i++) rows.add(new Object[] {"bulk-" + i});
    int[] inserted = jdbc.batchUpdate("INSERT INTO tags (name) VALUES (?)", rows);
    expect(inserted.length == 50, "batch returned " + inserted.length + " counts");
    expect(jdbc.queryForObject("SELECT COUNT(*) FROM tags WHERE name LIKE 'bulk-%'", Long.class) == 50, "batch insert count");
    tx.executeWithoutResult(s -> {
      for (Tag t : tags.findAll()) {
        if (t.name.startsWith("bulk-")) t.name = "batched-" + t.name.substring(5);
      }
    });
    expect(jdbc.queryForObject("SELECT COUNT(*) FROM tags WHERE name LIKE 'batched-%'", Long.class) == 50, "Hibernate batched updates");
    List<Long> ids = jdbc.queryForList("SELECT id FROM tags WHERE name LIKE 'batched-%'", Long.class);
    tags.deleteAllByIdInBatch(ids);
    expect(jdbc.queryForObject("SELECT COUNT(*) FROM tags WHERE name LIKE 'batched-%'", Long.class) == 0, "batch delete");
  }

  /** What schema tools read through Connector/J's DatabaseMetaData. */
  private void metadata() throws Exception {
    try (Connection c = DriverManager.getConnection(url, user, password)) {
      var md = c.getMetaData();
      Set<String> tables = new TreeSet<>();
      try (ResultSet rs = md.getTables(c.getCatalog(), null, "%", new String[] {"TABLE"})) {
        while (rs.next()) tables.add(rs.getString("TABLE_NAME"));
      }
      expect(tables.containsAll(List.of("users", "posts", "tags", "post_tags")), "getTables " + tables);
      Map<String, String> columns = new HashMap<>();
      try (ResultSet rs = md.getColumns(c.getCatalog(), null, "users", "%")) {
        while (rs.next()) columns.put(rs.getString("COLUMN_NAME"), rs.getString("TYPE_NAME") + "(" + rs.getInt("COLUMN_SIZE") + ")");
      }
      expect("VARCHAR(191)".equals(columns.get("email")) && "JSON".equals(columns.get("profile").replaceAll("\\(.*", "")), "getColumns " + columns);
      Set<String> keys = new TreeSet<>();
      try (ResultSet rs = md.getImportedKeys(c.getCatalog(), null, "post_tags")) {
        while (rs.next()) keys.add(rs.getString("FKCOLUMN_NAME") + "->" + rs.getString("PKTABLE_NAME") + "." + rs.getString("PKCOLUMN_NAME"));
      }
      expect(keys.equals(Set.of("post_id->posts.id", "tag_id->tags.id")), "getImportedKeys " + keys);
      Set<String> indexes = new TreeSet<>();
      try (ResultSet rs = md.getIndexInfo(c.getCatalog(), null, "users", true, false)) {
        while (rs.next()) indexes.add(rs.getString("INDEX_NAME") + ":" + rs.getString("COLUMN_NAME"));
      }
      expect(indexes.contains("uk_users_email:email") && indexes.contains("PRIMARY:id"), "getIndexInfo " + indexes);
      Set<String> pk = new TreeSet<>();
      try (ResultSet rs = md.getPrimaryKeys(c.getCatalog(), null, "post_tags")) {
        while (rs.next()) pk.add(rs.getString("COLUMN_NAME"));
      }
      expect(pk.equals(Set.of("post_id", "tag_id")), "getPrimaryKeys " + pk);
    }
  }

  private void alterMigration() throws Exception {
    MigrateResult result = flyway("2").migrate();
    expect(result.migrationsExecuted == 1, "migrated " + result.migrationsExecuted);
    Map<String, String> columns = columnsOfPosts();
    expect(columns.containsKey("slug") && columns.containsKey("view_count") && !columns.containsKey("views"), "posts columns " + columns);
    try (Connection c = DriverManager.getConnection(url, user, password);
        ResultSet rs = c.createStatement().executeQuery("SELECT COUNT(DISTINCT slug), SUM(view_count) FROM posts")) {
      rs.next();
      expect(rs.getInt(1) == 4 && rs.getInt(2) == 15, "slug/view_count " + rs.getInt(1) + " " + rs.getInt(2));
    }
  }

  private void revertMigration() throws Exception {
    MigrateResult result = flyway("3").migrate();
    expect(result.migrationsExecuted == 1, "migrated " + result.migrationsExecuted);
    Map<String, String> columns = columnsOfPosts();
    expect(!columns.containsKey("slug") && columns.containsKey("views"), "posts columns " + columns);
    // A fresh context validates the entities against the reverted schema again.
    if (ctx != null) ctx.close();
    ctx = null;
    start();
    expect(posts.count() == 4, "posts after the revert");
  }

  private void flywayClean() {
    if (ctx != null) ctx.close();
    ctx = null;
    flyway("3").clean();
    expect(tableNames().isEmpty(), "tables left after clean: " + tableNames());
  }

  private Map<String, String> columnsOfPosts() throws Exception {
    Map<String, String> columns = new HashMap<>();
    try (Connection c = DriverManager.getConnection(url, user, password);
        ResultSet rs = c.createStatement().executeQuery(
            "SELECT COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'posts'")) {
      while (rs.next()) columns.put(rs.getString(1), rs.getString(2));
    }
    return columns;
  }

  private List<String> tableNames() {
    List<String> names = new ArrayList<>();
    try (Connection c = DriverManager.getConnection(url, user, password);
        ResultSet rs = c.createStatement().executeQuery("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()")) {
      while (rs.next()) names.add(rs.getString(1));
    } catch (Exception e) {
      throw new IllegalStateException(e);
    }
    return names;
  }

  private void requireContext() {
    if (ctx == null) throw new IllegalStateException("the Spring context did not start");
  }

  private static void expect(boolean condition, String message) {
    if (!condition) throw new AssertionError(message);
  }
}
