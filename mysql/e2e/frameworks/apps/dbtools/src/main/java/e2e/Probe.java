package e2e;

import java.io.FileWriter;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.nio.charset.StandardCharsets;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;

/**
 * What database GUIs send when they connect and when a user browses a schema.
 *
 * <p>The jdbc-metadata steps are real: they call MySQL Connector/J's
 * DatabaseMetaData the way DBeaver (and Hibernate, IntelliJ, DbVisualizer,
 * Metabase...) do, once with the driver default (information_schema queries)
 * and once with useInformationSchema=false (SHOW statements). The dbeaver-,
 * workbench- and tableplus- steps replay the statements those tools are known
 * to issue, modeled on their MySQL general-log output; they are not captured
 * from the GUIs themselves. Every statement must work on MySQL 8.4.
 */
public final class Probe {
  private static final String DB = System.getenv("E2E_APP");

  public static void main(String[] args) throws Exception {
    step("setup", Probe::setup);
    step("jdbc-metadata", () -> metadata(url("")));
    step("jdbc-metadata-show", () -> metadata(url("&useInformationSchema=false")));
    step("jdbc-resultset-metadata", Probe::resultSetMetadata);
    step("dbeaver-connect", () -> replay(List.of(
        "SELECT @@version, @@version_comment",
        "SHOW ENGINES",
        "SHOW CHARSET",
        "SHOW COLLATION",
        "SHOW PLUGINS",
        "SHOW VARIABLES LIKE 'lower_case_table_names'",
        "SHOW VARIABLES LIKE 'character_set_server'",
        "SHOW VARIABLES LIKE 'sql_mode'",
        "SELECT @@GLOBAL.character_set_server, @@GLOBAL.collation_server",
        "SELECT DATABASE()",
        "SHOW DATABASES",
        "SELECT * FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME")));
    step("dbeaver-browse", () -> replay(List.of(
        "SHOW FULL TABLES FROM `" + DB + "`",
        "SHOW TABLE STATUS FROM `" + DB + "`",
        "SELECT * FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='" + DB + "' AND TABLE_NAME='users' ORDER BY ORDINAL_POSITION",
        "SELECT * FROM information_schema.STATISTICS WHERE TABLE_SCHEMA='" + DB + "' AND TABLE_NAME='posts' ORDER BY INDEX_NAME,SEQ_IN_INDEX",
        "SELECT * FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_SCHEMA='" + DB + "' AND TABLE_NAME='posts'",
        "SELECT kc.CONSTRAINT_NAME,kc.TABLE_NAME,kc.COLUMN_NAME,kc.ORDINAL_POSITION,kc.REFERENCED_TABLE_SCHEMA,"
            + "kc.REFERENCED_TABLE_NAME,kc.REFERENCED_COLUMN_NAME,rc.UPDATE_RULE,rc.DELETE_RULE"
            + " FROM information_schema.KEY_COLUMN_USAGE kc JOIN information_schema.REFERENTIAL_CONSTRAINTS rc"
            + " ON rc.CONSTRAINT_SCHEMA=kc.CONSTRAINT_SCHEMA AND rc.CONSTRAINT_NAME=kc.CONSTRAINT_NAME"
            + " WHERE kc.TABLE_SCHEMA='" + DB + "' AND kc.TABLE_NAME='posts' ORDER BY kc.CONSTRAINT_NAME,kc.ORDINAL_POSITION",
        "SELECT * FROM information_schema.CHECK_CONSTRAINTS WHERE CONSTRAINT_SCHEMA='" + DB + "'",
        "SHOW CREATE TABLE `" + DB + "`.`posts`",
        "SHOW CREATE VIEW `" + DB + "`.`active_users`",
        "SELECT * FROM information_schema.VIEWS WHERE TABLE_SCHEMA='" + DB + "'",
        "SELECT * FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA='" + DB + "'",
        "SELECT * FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA='" + DB + "' ORDER BY ROUTINE_NAME",
        "SELECT * FROM information_schema.EVENTS WHERE EVENT_SCHEMA='" + DB + "'",
        "SELECT * FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA='" + DB + "' AND TABLE_NAME='users'")));
    step("dbeaver-data", () -> replay(List.of(
        "SELECT * FROM `" + DB + "`.`users` LIMIT 0, 200",
        "SELECT COUNT(*) FROM `" + DB + "`.`users`",
        "SELECT * FROM `" + DB + "`.`active_users` LIMIT 0, 200",
        "SELECT * FROM `" + DB + "`.`posts` WHERE `user_id` = 1 LIMIT 0, 200",
        "SHOW FULL PROCESSLIST")));
    step("workbench-connect", () -> replay(List.of(
        "SET NAMES 'utf8mb4'",
        "SET @@SESSION.autocommit = ON",
        "SELECT current_user()",
        "SET SQL_SAFE_UPDATES=1",
        "SELECT CONNECTION_ID()",
        "SHOW SESSION VARIABLES LIKE 'lower_case_table_names'",
        "SHOW SESSION VARIABLES LIKE 'version_comment'",
        "SHOW SESSION VARIABLES LIKE 'version'",
        "SHOW SESSION STATUS LIKE 'Ssl_cipher'",
        "SELECT st.* FROM performance_schema.events_statements_current st JOIN performance_schema.threads thr"
            + " ON thr.thread_id = st.thread_id WHERE thr.processlist_id = CONNECTION_ID()",
        "SHOW DATABASES")));
    step("workbench-browse", () -> replay(List.of(
        "SHOW FULL TABLES FROM `" + DB + "`",
        "SHOW COLUMNS FROM `" + DB + "`.`users`",
        "SHOW INDEX FROM `" + DB + "`.`users`",
        "SHOW TRIGGERS FROM `" + DB + "`",
        "SHOW PROCEDURE STATUS WHERE Db = '" + DB + "'",
        "SHOW FUNCTION STATUS WHERE Db = '" + DB + "'",
        "SHOW CREATE TABLE `" + DB + "`.`users`",
        "SELECT * FROM `" + DB + "`.`users` LIMIT 0, 1000",
        "EXPLAIN FORMAT=JSON SELECT * FROM `" + DB + "`.`posts` WHERE user_id = 1",
        "EXPLAIN SELECT * FROM `" + DB + "`.`posts` WHERE user_id = 1")));
    step("tableplus-connect", () -> replay(List.of(
        "SHOW VARIABLES LIKE '%version%'",
        "SELECT @@SESSION.sql_mode",
        "SHOW DATABASES",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA")));
    step("tableplus-browse", () -> replay(List.of(
        "SELECT TABLE_NAME AS name, TABLE_TYPE AS type, ENGINE AS engine, TABLE_ROWS AS table_rows,"
            + " TABLE_COMMENT AS comment FROM information_schema.TABLES WHERE TABLE_SCHEMA = '" + DB + "' ORDER BY TABLE_NAME",
        "SHOW FULL COLUMNS FROM `" + DB + "`.`users`",
        "SHOW INDEX FROM `" + DB + "`.`users`",
        "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME"
            + " FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = '" + DB + "' AND TABLE_NAME = 'posts'"
            + " AND REFERENCED_TABLE_NAME IS NOT NULL",
        "SHOW TABLE STATUS FROM `" + DB + "` WHERE Name = 'users'",
        "SHOW CREATE TABLE `" + DB + "`.`users`",
        "SELECT * FROM `" + DB + "`.`users` LIMIT 300 OFFSET 0",
        "SELECT COUNT(*) FROM `" + DB + "`.`users`",
        "SHOW PROCESSLIST")));
    System.exit(0);
  }

  private static String url(String extra) {
    return "jdbc:mysql://" + System.getenv("E2E_HOST") + ":" + System.getenv("E2E_PORT") + "/" + DB
        + "?sslMode=VERIFY_IDENTITY&trustCertificateKeyStoreUrl=file:/tmp/ca.p12"
        + "&trustCertificateKeyStoreType=PKCS12&trustCertificateKeyStorePassword=changeit" + extra;
  }

  private static Connection connect(String url) throws SQLException {
    return DriverManager.getConnection(url, System.getenv("E2E_USER"), System.getenv("E2E_PASSWORD"));
  }

  private static void setup() throws Exception {
    try (Connection c = connect(url("")); Statement s = c.createStatement()) {
      for (String sql : List.of(
          "CREATE TABLE users (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, email VARCHAR(191) NOT NULL,"
              + " name VARCHAR(100) NOT NULL, balance DECIMAL(10,2) NOT NULL DEFAULT 0, is_active TINYINT(1) NOT NULL DEFAULT 1,"
              + " profile JSON NULL, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, UNIQUE KEY users_email (email))",
          "CREATE TABLE posts (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, user_id BIGINT NOT NULL, title VARCHAR(200) NOT NULL,"
              + " body TEXT NULL, views INT NOT NULL DEFAULT 0, KEY posts_user (user_id),"
              + " CONSTRAINT posts_user_fk FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE)",
          "CREATE VIEW active_users AS SELECT id, email, name FROM users WHERE is_active = 1",
          "INSERT INTO users (email, name, balance, profile) VALUES ('alice@example.com', 'Alice', 100.50, '{\"city\": \"Tokyo\"}'),"
              + " ('bob@example.com', 'Bob', 20.25, NULL)",
          "INSERT INTO posts (user_id, title, body, views) VALUES (1, 'Hello', 'First post', 10), (1, 'Draft', NULL, 0)")) {
        s.execute(sql);
      }
    }
  }

  /** DatabaseMetaData calls a schema browser makes, checked against the setup schema. */
  private static void metadata(String url) throws Exception {
    List<String> failures = new ArrayList<>();
    try (Connection c = connect(url)) {
      DatabaseMetaData md = c.getMetaData();
      System.out.println(md.getDatabaseProductName() + " " + md.getDatabaseProductVersion() + ", " + md.getDriverName() + " " + md.getDriverVersion());
      check(failures, "getCatalogs", () -> names(md.getCatalogs(), "TABLE_CAT").contains(DB));
      check(failures, "getSchemas", () -> {
        md.getSchemas().close();
        return true;
      });
      check(failures, "getTableTypes", () -> names(md.getTableTypes(), "TABLE_TYPE").contains("TABLE"));
      check(failures, "getTypeInfo", () -> names(md.getTypeInfo(), "TYPE_NAME").contains("VARCHAR"));
      check(failures, "getSQLKeywords", () -> md.getSQLKeywords().length() > 0);
      check(failures, "getTables", () -> names(md.getTables(DB, null, "%", null), "TABLE_NAME").containsAll(List.of("users", "posts", "active_users")));
      check(failures, "getTables(VIEW)", () -> names(md.getTables(DB, null, "%", new String[] {"VIEW"}), "TABLE_NAME").equals(Set.of("active_users")));
      check(failures, "getColumns", () -> {
        Set<String> columns = new TreeSet<>();
        try (ResultSet rs = md.getColumns(DB, null, "users", "%")) {
          while (rs.next()) columns.add(rs.getString("COLUMN_NAME") + ":" + rs.getString("TYPE_NAME") + ":" + rs.getInt("NULLABLE") + ":" + rs.getString("IS_AUTOINCREMENT"));
        }
        System.out.println(columns);
        return columns.contains("id:BIGINT:0:YES") && columns.contains("email:VARCHAR:0:NO") && columns.contains("profile:JSON:1:NO");
      });
      check(failures, "getPrimaryKeys", () -> names(md.getPrimaryKeys(DB, null, "posts"), "COLUMN_NAME").equals(Set.of("id")));
      check(failures, "getImportedKeys", () -> names(md.getImportedKeys(DB, null, "posts"), "PKTABLE_NAME").equals(Set.of("users")));
      check(failures, "getExportedKeys", () -> names(md.getExportedKeys(DB, null, "users"), "FKTABLE_NAME").equals(Set.of("posts")));
      check(failures, "getCrossReference", () -> names(md.getCrossReference(DB, null, "users", DB, null, "posts"), "FKCOLUMN_NAME").equals(Set.of("user_id")));
      check(failures, "getIndexInfo", () -> names(md.getIndexInfo(DB, null, "users", false, false), "INDEX_NAME").containsAll(List.of("PRIMARY", "users_email")));
      check(failures, "getBestRowIdentifier", () -> names(md.getBestRowIdentifier(DB, null, "users", DatabaseMetaData.bestRowSession, true), "COLUMN_NAME").equals(Set.of("id")));
      check(failures, "getProcedures", () -> names(md.getProcedures(DB, null, "%"), "PROCEDURE_NAME").isEmpty());
      check(failures, "getFunctions", () -> names(md.getFunctions(DB, null, "%"), "FUNCTION_NAME").isEmpty());
      check(failures, "getTablePrivileges", () -> {
        md.getTablePrivileges(DB, null, "users").close();
        return true;
      });
    }
    if (!failures.isEmpty()) throw new AssertionError(String.join("\n", failures));
  }

  /** What a result grid reads for each column: type, size, nullability, auto-increment, table. */
  private static void resultSetMetadata() throws Exception {
    try (Connection c = connect(url("")); Statement s = c.createStatement();
        ResultSet rs = s.executeQuery("SELECT u.id, u.email, u.balance, u.is_active, u.profile, u.created_at, p.title FROM users u JOIN posts p ON p.user_id = u.id")) {
      ResultSetMetaData m = rs.getMetaData();
      List<String> columns = new ArrayList<>();
      for (int i = 1; i <= m.getColumnCount(); i++) {
        columns.add(m.getTableName(i) + "." + m.getColumnName(i) + ":" + m.getColumnTypeName(i) + "(" + m.getPrecision(i) + "," + m.getScale(i) + ")"
            + (m.isNullable(i) == ResultSetMetaData.columnNoNulls ? " not null" : "") + (m.isAutoIncrement(i) ? " auto" : ""));
      }
      System.out.println(columns);
      List<String> expected = List.of(
          "users.id:BIGINT(19,0) not null auto", "users.email:VARCHAR(191,0) not null", "users.balance:DECIMAL(10,2) not null",
          "users.is_active:BIT(1,0) not null", "users.profile:JSON(2147483647,0)", "users.created_at:DATETIME(19,0) not null",
          "posts.title:VARCHAR(200,0) not null");
      if (!columns.equals(expected)) throw new AssertionError("result set metadata " + columns + "\nexpected " + expected);
    }
  }

  /** Runs each statement the way a GUI does and reads its whole result; reports every failure. */
  private static void replay(List<String> statements) throws Exception {
    List<String> failures = new ArrayList<>();
    try (Connection c = connect(url("")); Statement s = c.createStatement()) {
      for (String sql : statements) {
        try {
          if (s.execute(sql)) {
            try (ResultSet rs = s.getResultSet()) {
              int rows = 0;
              int columns = rs.getMetaData().getColumnCount();
              while (rs.next()) {
                for (int i = 1; i <= columns; i++) rs.getObject(i);
                rows++;
              }
              System.out.println(sql + " -> " + rows + " rows");
            }
          } else {
            System.out.println(sql + " -> ok");
          }
        } catch (SQLException e) {
          failures.add(e.getErrorCode() + " " + e.getMessage() + ": " + sql);
        }
      }
    }
    if (!failures.isEmpty()) throw new AssertionError(failures.size() + " of " + statements.size() + " statements failed:\n" + String.join("\n", failures));
  }

  private static Set<String> names(ResultSet rs, String column) throws SQLException {
    Set<String> names = new TreeSet<>();
    try (rs) {
      while (rs.next()) names.add(rs.getString(column));
    }
    return names;
  }

  interface Check {
    boolean ok() throws Exception;
  }

  private static void check(List<String> failures, String name, Check check) {
    try {
      if (!check.ok()) failures.add(name + ": unexpected result");
    } catch (Exception e) {
      failures.add(name + ": " + e);
    }
  }

  interface Body {
    void run() throws Exception;
  }

  private static void step(String name, Body body) {
    System.out.println("=== step " + name);
    String line;
    try {
      body.run();
      line = "{\"step\":\"" + name + "\",\"ok\":true}";
    } catch (Throwable e) {
      StringWriter trace = new StringWriter();
      e.printStackTrace(new PrintWriter(trace));
      String text = trace.toString();
      System.out.println(text);
      line = "{\"step\":\"" + name + "\",\"ok\":false,\"error\":" + jsonString(text.substring(Math.max(0, text.length() - 2000))) + "}";
    }
    try (FileWriter out = new FileWriter(System.getenv("E2E_OUT") + "/steps.jsonl", StandardCharsets.UTF_8, true)) {
      out.write(line + "\n");
    } catch (Exception e) {
      throw new IllegalStateException(e);
    }
  }

  private static String jsonString(String s) {
    StringBuilder b = new StringBuilder("\"");
    for (char ch : s.toCharArray()) {
      switch (ch) {
        case '"' -> b.append("\\\"");
        case '\\' -> b.append("\\\\");
        case '\n' -> b.append("\\n");
        case '\r' -> b.append("\\r");
        case '\t' -> b.append("\\t");
        default -> {
          if (ch < 0x20) b.append(String.format("\\u%04x", (int) ch));
          else b.append(ch);
        }
      }
    }
    return b.append('"').toString();
  }
}
