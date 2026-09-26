import jakarta.persistence.Column;
import jakarta.persistence.Entity;
import jakarta.persistence.FetchType;
import jakarta.persistence.Id;
import jakarta.persistence.JoinColumn;
import jakarta.persistence.ManyToOne;
import jakarta.persistence.Table;
import java.io.InputStream;
import java.math.BigDecimal;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.cert.CertificateFactory;
import java.sql.DatabaseMetaData;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.time.LocalDateTime;
import org.hibernate.Session;
import org.hibernate.SessionFactory;
import org.hibernate.Transaction;
import org.hibernate.boot.MetadataSources;
import org.hibernate.boot.registry.StandardServiceRegistry;
import org.hibernate.boot.registry.StandardServiceRegistryBuilder;

public final class HibernateE2E {
    private static final String DATABASE = "reports";
    private static final BigDecimal INITIAL_AMOUNT = new BigDecimal("1.234567890123456789");
    private static final BigDecimal UPDATED_AMOUNT = new BigDecimal("2.000000000000000001");
    private static final LocalDateTime CREATED_AT = LocalDateTime.of(2030, 5, 6, 7, 8, 9);

    public static void main(String[] args) throws Exception {
        if (args.length > 1 || (args.length == 1 && !args[0].equals("prepare") && !args[0].equals("verify"))) {
            throw new IllegalArgumentException("expected prepare or verify");
        }
        Path ca = Path.of(required("TURSO_MYSQL_DRIVER_CA"));
        Path trustStore = Files.createTempFile(ca.getParent(), "turso-hibernate-", ".jks");
        try {
            createTrustStore(ca, trustStore);
            String url = "jdbc:mysql://" + required("TURSO_MYSQL_DRIVER_ENDPOINT") + "/" + DATABASE
                    + "?sslMode=VERIFY_IDENTITY&trustCertificateKeyStoreUrl="
                    + URLEncoder.encode(trustStore.toUri().toString(), StandardCharsets.UTF_8)
                    + "&trustCertificateKeyStorePassword=fixture-password&useInformationSchema=false";
            String user = System.getenv().getOrDefault("TURSO_MYSQL_DRIVER_USER", "gateadmin");
            String password = required("TURSO_MYSQL_DRIVER_PASSWORD");

            if (args.length == 0 || args[0].equals("prepare")) {
                prepare(url, user, password);
            }
            if (args.length == 0 || args[0].equals("verify")) {
                verify(url, user, password);
            }
            stage("complete");
        } finally {
            Files.deleteIfExists(trustStore);
        }
    }

    private static void prepare(String url, String user, String password) {
        stage("create schema");
        try (SessionFactory ignored = openFactory(url, user, password, "create-only", AccountV1.class)) {}

        stage("update schema");
        try (SessionFactory ignored = openFactory(url, user, password, "update", Account.class, Line.class)) {}

        stage("validate schema before restart");
        try (SessionFactory factory = openFactory(url, user, password, "validate", Account.class, Line.class)) {
            inspectSchema(factory);
            writeInitialRows(factory);
        }
    }

    private static void verify(String url, String user, String password) {
        stage("validate schema after restart");
        try (SessionFactory factory = openFactory(url, user, password, "validate", Account.class, Line.class)) {
            inspectSchema(factory);
            readAndUpdate(factory);
            checkRollback(factory);
            checkConstraints(factory);
            deleteRows(factory);
        }

        stage("validate schema after pool reopen");
        try (SessionFactory factory = openFactory(url, user, password, "validate", Account.class, Line.class)) {
            inspectSchema(factory);
        }

        stage("drop schema");
        try (SessionFactory ignored = openFactory(url, user, password, "drop", Account.class, Line.class)) {}
        stage("verify schema removal");
        try (SessionFactory factory = openFactory(url, user, password, "none", Account.class, Line.class)) {
            try (Session session = factory.openSession()) {
                session.doWork(connection -> {
                    try (ResultSet tables = connection.getMetaData().getTables(DATABASE, null, "hibernate_e2e_account", null)) {
                        require(!tables.next(), "dropped account table is still visible");
                    }
                });
            }
        }
    }

    private static SessionFactory openFactory(String url, String user, String password, String schemaAction,
                                               Class<?>... entities) {
        StandardServiceRegistry registry = new StandardServiceRegistryBuilder()
                .applySetting("hibernate.connection.driver_class", "com.mysql.cj.jdbc.Driver")
                .applySetting("hibernate.connection.url", url)
                .applySetting("hibernate.connection.username", user)
                .applySetting("hibernate.connection.password", password)
                .applySetting("hibernate.connection.pool_size", "2")
                .applySetting("hibernate.hbm2ddl.auto", schemaAction)
                .applySetting("hibernate.hbm2ddl.halt_on_error", "true")
                .applySetting("hibernate.show_sql", "true")
                .build();
        try {
            MetadataSources sources = new MetadataSources(registry);
            for (Class<?> entity : entities) {
                sources.addAnnotatedClass(entity);
            }
            return sources.buildMetadata().buildSessionFactory();
        } catch (RuntimeException | Error e) {
            StandardServiceRegistryBuilder.destroy(registry);
            throw e;
        }
    }

    private static void inspectSchema(SessionFactory factory) {
        try (Session session = factory.openSession()) {
            session.doWork(connection -> {
                DatabaseMetaData metadata = connection.getMetaData();
                boolean amount = false;
                boolean note = false;
                try (ResultSet columns = metadata.getColumns(DATABASE, null, "hibernate_e2e_account", "%")) {
                    while (columns.next()) {
                        String name = columns.getString("COLUMN_NAME");
                        amount |= "amount".equalsIgnoreCase(name)
                                && columns.getInt("DECIMAL_DIGITS") == 18;
                        note |= "note".equalsIgnoreCase(name)
                                && columns.getInt("NULLABLE") == DatabaseMetaData.columnNullable;
                    }
                }
                require(amount && note, "migration columns are missing or have wrong metadata");
                try (ResultSet foreignKeys = metadata.getImportedKeys(DATABASE, null, "hibernate_e2e_line")) {
                    boolean found = false;
                    while (foreignKeys.next()) {
                        found |= "account_id".equalsIgnoreCase(foreignKeys.getString("FKCOLUMN_NAME"))
                                && "hibernate_e2e_account".equalsIgnoreCase(foreignKeys.getString("PKTABLE_NAME"));
                    }
                    require(found, "line foreign key is missing from metadata");
                }
            });
        }
    }

    private static void writeInitialRows(SessionFactory factory) {
        try (Session session = factory.openSession()) {
            Transaction transaction = session.beginTransaction();
            Account account = new Account(1, "alpha", INITIAL_AMOUNT, CREATED_AT);
            session.persist(account);
            session.persist(new Line(10, account));
            transaction.commit();
        }
    }

    private static void readAndUpdate(SessionFactory factory) {
        try (Session session = factory.openSession()) {
            Line line = session.createQuery(
                    "select line from HibernateLine line join fetch line.account where line.id = :id", Line.class)
                    .setParameter("id", 10)
                    .getSingleResult();
            require(line.account.id == 1 && line.account.note == null, "relation or NULL value changed");
            require(INITIAL_AMOUNT.equals(line.account.amount), "DECIMAL value changed");
            require(CREATED_AT.equals(line.account.createdAt), "TIMESTAMP value changed");
        }
        try (Session session = factory.openSession()) {
            Transaction transaction = session.beginTransaction();
            Account account = session.find(Account.class, 1);
            account.note = "migrated";
            account.amount = UPDATED_AMOUNT;
            transaction.commit();
        }
        try (Session session = factory.openSession()) {
            Account account = session.find(Account.class, 1);
            require("migrated".equals(account.note), "updated note changed");
            require(UPDATED_AMOUNT.equals(account.amount), "updated DECIMAL value changed");
        }
    }

    private static void checkRollback(SessionFactory factory) {
        try (Session session = factory.openSession()) {
            Transaction transaction = session.beginTransaction();
            session.find(Account.class, 1).note = "discard";
            session.flush();
            transaction.rollback();
        }
        try (Session session = factory.openSession()) {
            require("migrated".equals(session.find(Account.class, 1).note), "rollback did not restore note");
        }
    }

    private static void checkConstraints(SessionFactory factory) {
        expectConstraint(factory, session -> session.persist(new Account(2, "alpha", INITIAL_AMOUNT, CREATED_AT)));
        expectConstraint(factory, session -> session.persist(new Line(11, session.getReference(Account.class, 999))));
    }

    private static void expectConstraint(SessionFactory factory, Action action) {
        try (Session session = factory.openSession()) {
            Transaction transaction = session.beginTransaction();
            try {
                action.run(session);
                transaction.commit();
                throw new AssertionError("expected unique or foreign key failure");
            } catch (RuntimeException e) {
                if (transaction.isActive()) {
                    transaction.rollback();
                }
                require(isIntegrityError(e), "expected SQLSTATE 23000, got " + e);
            }
        }
    }

    private static boolean isIntegrityError(Throwable error) {
        for (Throwable cause = error; cause != null; cause = cause.getCause()) {
            if (cause instanceof SQLException sql && "23000".equals(sql.getSQLState())) {
                return true;
            }
        }
        return false;
    }

    private static void deleteRows(SessionFactory factory) {
        try (Session session = factory.openSession()) {
            Transaction transaction = session.beginTransaction();
            session.remove(session.find(Line.class, 10));
            session.remove(session.find(Account.class, 1));
            transaction.commit();
        }
        try (Session session = factory.openSession()) {
            require(session.find(Line.class, 10) == null, "deleted line is still visible");
            require(session.find(Account.class, 1) == null, "deleted account is still visible");
        }
    }

    private static void createTrustStore(Path ca, Path path) throws Exception {
        KeyStore store = KeyStore.getInstance("JKS");
        store.load(null);
        try (InputStream input = Files.newInputStream(ca)) {
            store.setCertificateEntry("fixture", CertificateFactory.getInstance("X.509").generateCertificate(input));
        }
        try (var output = Files.newOutputStream(path)) {
            store.store(output, "fixture-password".toCharArray());
        }
    }

    private static String required(String name) {
        String value = System.getenv(name);
        require(value != null && !value.isEmpty(), "missing " + name);
        return value;
    }

    private static void stage(String name) {
        System.out.println("Hibernate E2E: " + name);
    }

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    @FunctionalInterface
    private interface Action {
        void run(Session session);
    }

    @Entity(name = "HibernateAccountV1")
    @Table(name = "hibernate_e2e_account")
    public static class AccountV1 {
        @Id
        public Integer id;

        @Column(nullable = false, unique = true, length = 64)
        public String name;

        @Column(precision = 30, scale = 18)
        public BigDecimal amount;

        @Column(name = "created_at", columnDefinition = "timestamp")
        public LocalDateTime createdAt;

        protected AccountV1() {}
    }

    @Entity(name = "HibernateAccount")
    @Table(name = "hibernate_e2e_account")
    public static class Account {
        @Id
        public Integer id;

        @Column(nullable = false, unique = true, length = 64)
        public String name;

        @Column(precision = 30, scale = 18)
        public BigDecimal amount;

        @Column(name = "created_at", columnDefinition = "timestamp")
        public LocalDateTime createdAt;

        @Column(length = 64)
        public String note;

        protected Account() {}

        Account(int id, String name, BigDecimal amount, LocalDateTime createdAt) {
            this.id = id;
            this.name = name;
            this.amount = amount;
            this.createdAt = createdAt;
        }
    }

    @Entity(name = "HibernateLine")
    @Table(name = "hibernate_e2e_line")
    public static class Line {
        @Id
        public Integer id;

        @ManyToOne(fetch = FetchType.LAZY, optional = false)
        @JoinColumn(name = "account_id", nullable = false)
        public Account account;

        protected Line() {}

        Line(int id, Account account) {
            this.id = id;
            this.account = account;
        }
    }
}
