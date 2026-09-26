import java.io.InputStream;
import java.math.BigDecimal;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.cert.CertificateFactory;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;
import java.util.Properties;

public final class JdbcDriver {
    public static void main(String[] args) throws Exception {
        Path caPath = Path.of(System.getenv("TURSO_MYSQL_DRIVER_CA"));
        Path trustStore = Files.createTempFile(caPath.getParent(), "turso-mysql-driver-", ".jks");
        try {
            KeyStore store = KeyStore.getInstance("JKS");
            store.load(null);
            try (InputStream ca = Files.newInputStream(caPath)) {
                store.setCertificateEntry("fixture", CertificateFactory.getInstance("X.509").generateCertificate(ca));
            }
            try (var output = Files.newOutputStream(trustStore)) {
                store.store(output, "fixture-password".toCharArray());
            }

            Properties properties = new Properties();
            properties.setProperty("user", "gateadmin");
            properties.setProperty("password", System.getenv("TURSO_MYSQL_DRIVER_PASSWORD"));
            properties.setProperty("sslMode", "VERIFY_IDENTITY");
            properties.setProperty("trustCertificateKeyStoreUrl", trustStore.toUri().toString());
            properties.setProperty("trustCertificateKeyStorePassword", "fixture-password");
            properties.setProperty("useServerPrepStmts", "true");
            properties.setProperty("useInformationSchema", "false");
            properties.setProperty("allowMultiQueries", "true");
            properties.setProperty("connectTimeout", "3000");
            properties.setProperty("socketTimeout", "3000");
            String url = "jdbc:mysql://" + System.getenv("TURSO_MYSQL_DRIVER_ENDPOINT") + "/reports";
            try (Connection connection = DriverManager.getConnection(url, properties);
                 Statement statement = connection.createStatement()) {
                if (!statement.execute("SELECT 1; SELECT 2")) throw new AssertionError("first multi result");
                try (ResultSet first = statement.getResultSet()) {
                    if (!first.next() || first.getInt(1) != 1 || first.next()) {
                        throw new AssertionError("first multi result value");
                    }
                }
                if (!statement.getMoreResults()) throw new AssertionError("second multi result");
                try (ResultSet second = statement.getResultSet()) {
                    if (!second.next() || second.getInt(1) != 2 || second.next()) {
                        throw new AssertionError("second multi result value");
                    }
                }
                if (statement.getMoreResults()) throw new AssertionError("unexpected third multi result");
                statement.executeUpdate("CREATE TABLE jdbc_records (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) NOT NULL)");
                try (PreparedStatement insert = connection.prepareStatement("INSERT INTO jdbc_records (id, name) VALUES (?, ?)")) {
                    insert.setInt(1, 1);
                    insert.setString(2, "alpha");
                    if (insert.executeUpdate() != 1) throw new AssertionError("insert count");
                }
                assertName(statement, "alpha");
                connection.setAutoCommit(false);
                statement.executeUpdate("UPDATE jdbc_records SET name = 'beta' WHERE id = 1");
                connection.rollback();
                connection.setAutoCommit(true);
                assertName(statement, "alpha");
                statement.executeUpdate("ALTER TABLE jdbc_records ADD COLUMN note VARCHAR(20)");
                try (ResultSet tables = statement.executeQuery("SHOW FULL TABLES FROM `reports` LIKE 'jdbc_records'")) {
                    if (!tables.next()) throw new AssertionError("SHOW FULL TABLES missed migrated table");
                }
                try (ResultSet columns = connection.getMetaData().getColumns("reports", null, "jdbc_records", "%")) {
                    boolean name = false;
                    boolean note = false;
                    while (columns.next()) {
                        if ("name".equals(columns.getString("COLUMN_NAME"))) name = columns.getInt("COLUMN_SIZE") == 20;
                        if ("note".equals(columns.getString("COLUMN_NAME"))) note = columns.getInt("COLUMN_SIZE") == 20;
                    }
                    if (!name || !note) throw new AssertionError("migration metadata");
                }
                if (statement.executeUpdate("DELETE FROM jdbc_records WHERE id = 1") != 1) {
                    throw new AssertionError("delete count");
                }
                statement.executeUpdate("DROP TABLE jdbc_records");
                statement.executeUpdate("CREATE TABLE jdbc_values (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) UNIQUE, amount DECIMAL(65,30))");
                try (PreparedStatement insert = connection.prepareStatement("INSERT INTO jdbc_values (id, name, amount) VALUES (?, ?, ?)")) {
                    insert.setInt(1, 1);
                    insert.setString(2, "café");
                    insert.setBigDecimal(3, new BigDecimal("1.234567890123456789012345678901"));
                    if (insert.executeUpdate() != 1) throw new AssertionError("exact value insert count");
                }
                try (PreparedStatement select = connection.prepareStatement("SELECT amount FROM jdbc_values WHERE name = ?")) {
                    select.setString(1, "CAFE");
                    try (ResultSet values = select.executeQuery()) {
                        BigDecimal expected = new BigDecimal("1.234567890123456789012345678901");
                        if (!values.next() || !expected.equals(values.getBigDecimal(1)) || values.next()) {
                            throw new AssertionError("exact decimal or Unicode collation");
                        }
                    }
                }
                statement.executeUpdate("DROP TABLE jdbc_values");
            }
        } finally {
            Files.deleteIfExists(trustStore);
        }
    }

    private static void assertName(Statement statement, String expected) throws Exception {
        try (ResultSet rows = statement.executeQuery("SELECT name FROM jdbc_records WHERE id = 1")) {
            if (!rows.next() || !expected.equals(rows.getString(1)) || rows.next()) {
                throw new AssertionError("unexpected row");
            }
        }
    }
}
