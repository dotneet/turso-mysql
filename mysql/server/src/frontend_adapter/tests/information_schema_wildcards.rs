//! `SELECT *` over the `information_schema` tables, which schema-sync ORMs,
//! `prisma db pull`, Doctrine and GUI tools send, and the exact catalog reads
//! TypeORM, Sequelize and Doctrine DBAL make.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([151; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
         email VARCHAR(255) NOT NULL, age INT, UNIQUE KEY uk_email (email))",
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
         user_id INT NOT NULL, title VARCHAR(100), \
         CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn read(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, not {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    read(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn names(result: &TextResultSet) -> Vec<&str> {
    result
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect()
}

fn row(values: &[Option<&str>]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| value.map(str::to_owned))
        .collect()
}

/// A wildcard asks for every column MySQL gives the table, in the order MySQL
/// declares them, and each of these answers all of them.
#[test]
fn a_wildcard_answers_every_column_mysql_has_in_its_order() {
    let (_directory, mut adapter) = adapter();
    for (sql, expected) in [
        (
            "SELECT * FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users'",
            &[
                "TABLE_CATALOG",
                "TABLE_SCHEMA",
                "TABLE_NAME",
                "COLUMN_NAME",
                "ORDINAL_POSITION",
                "COLUMN_DEFAULT",
                "IS_NULLABLE",
                "DATA_TYPE",
                "CHARACTER_MAXIMUM_LENGTH",
                "CHARACTER_OCTET_LENGTH",
                "NUMERIC_PRECISION",
                "NUMERIC_SCALE",
                "DATETIME_PRECISION",
                "CHARACTER_SET_NAME",
                "COLLATION_NAME",
                "COLUMN_TYPE",
                "COLUMN_KEY",
                "EXTRA",
                "PRIVILEGES",
                "COLUMN_COMMENT",
                "GENERATION_EXPRESSION",
                "SRS_ID",
            ][..],
        ),
        (
            "SELECT * FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_NAME IS NOT NULL",
            &[
                "CONSTRAINT_CATALOG",
                "CONSTRAINT_SCHEMA",
                "CONSTRAINT_NAME",
                "TABLE_CATALOG",
                "TABLE_SCHEMA",
                "TABLE_NAME",
                "COLUMN_NAME",
                "ORDINAL_POSITION",
                "POSITION_IN_UNIQUE_CONSTRAINT",
                "REFERENCED_TABLE_SCHEMA",
                "REFERENCED_TABLE_NAME",
                "REFERENCED_COLUMN_NAME",
            ],
        ),
        (
            "SELECT * FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = DATABASE()",
            &[
                "TABLE_CATALOG",
                "TABLE_SCHEMA",
                "TABLE_NAME",
                "NON_UNIQUE",
                "INDEX_SCHEMA",
                "INDEX_NAME",
                "SEQ_IN_INDEX",
                "COLUMN_NAME",
                "COLLATION",
                "CARDINALITY",
                "SUB_PART",
                "PACKED",
                "NULLABLE",
                "INDEX_TYPE",
                "COMMENT",
                "INDEX_COMMENT",
                "IS_VISIBLE",
                "EXPRESSION",
            ],
        ),
        (
            "SELECT * FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = DATABASE()",
            &[
                "CONSTRAINT_CATALOG",
                "CONSTRAINT_SCHEMA",
                "CONSTRAINT_NAME",
                "UNIQUE_CONSTRAINT_CATALOG",
                "UNIQUE_CONSTRAINT_SCHEMA",
                "UNIQUE_CONSTRAINT_NAME",
                "MATCH_OPTION",
                "UPDATE_RULE",
                "DELETE_RULE",
                "TABLE_NAME",
                "REFERENCED_TABLE_NAME",
            ],
        ),
        (
            "SELECT * FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_SCHEMA = DATABASE()",
            &[
                "CONSTRAINT_CATALOG",
                "CONSTRAINT_SCHEMA",
                "CONSTRAINT_NAME",
                "TABLE_SCHEMA",
                "TABLE_NAME",
                "CONSTRAINT_TYPE",
                "ENFORCED",
            ],
        ),
        (
            "SELECT * FROM information_schema.SCHEMATA",
            &[
                "CATALOG_NAME",
                "SCHEMA_NAME",
                "DEFAULT_CHARACTER_SET_NAME",
                "DEFAULT_COLLATION_NAME",
                "SQL_PATH",
                "DEFAULT_ENCRYPTION",
            ],
        ),
        (
            "SELECT * FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE()",
            &[
                "SPECIFIC_NAME",
                "ROUTINE_CATALOG",
                "ROUTINE_SCHEMA",
                "ROUTINE_NAME",
                "ROUTINE_TYPE",
                "DATA_TYPE",
                "CHARACTER_MAXIMUM_LENGTH",
                "CHARACTER_OCTET_LENGTH",
                "NUMERIC_PRECISION",
                "NUMERIC_SCALE",
                "DATETIME_PRECISION",
                "CHARACTER_SET_NAME",
                "COLLATION_NAME",
                "DTD_IDENTIFIER",
                "ROUTINE_BODY",
                "ROUTINE_DEFINITION",
                "EXTERNAL_NAME",
                "EXTERNAL_LANGUAGE",
                "PARAMETER_STYLE",
                "IS_DETERMINISTIC",
                "SQL_DATA_ACCESS",
                "SQL_PATH",
                "SECURITY_TYPE",
                "CREATED",
                "LAST_ALTERED",
                "SQL_MODE",
                "ROUTINE_COMMENT",
                "DEFINER",
                "CHARACTER_SET_CLIENT",
                "COLLATION_CONNECTION",
                "DATABASE_COLLATION",
            ],
        ),
    ] {
        let result = read(&mut adapter, sql);
        assert_eq!(names(&result), expected, "{sql}");
        assert!(
            result.rows.iter().all(|row| row.len() == expected.len()),
            "{sql}"
        );
    }
}

/// The rows MySQL answers for the same two tables.
#[test]
fn a_wildcard_answers_the_rows_mysql_answers() {
    let (_directory, mut adapter) = adapter();
    let all = Some("select,insert,update,references");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users' ORDER BY ORDINAL_POSITION",
        ),
        [
            row(&[
                Some("def"),
                Some("reports"),
                Some("users"),
                Some("id"),
                Some("1"),
                None,
                Some("NO"),
                Some("int"),
                None,
                None,
                Some("10"),
                Some("0"),
                None,
                None,
                None,
                Some("int"),
                Some("PRI"),
                Some("auto_increment"),
                all,
                Some(""),
                Some(""),
                None,
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("users"),
                Some("email"),
                Some("2"),
                None,
                Some("NO"),
                Some("varchar"),
                Some("255"),
                Some("1020"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                Some("varchar(255)"),
                Some("UNI"),
                Some(""),
                all,
                Some(""),
                Some(""),
                None,
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("users"),
                Some("age"),
                Some("3"),
                None,
                Some("YES"),
                Some("int"),
                None,
                None,
                Some("10"),
                Some("0"),
                None,
                None,
                None,
                Some("int"),
                Some(""),
                Some(""),
                all,
                Some(""),
                Some(""),
                None,
            ]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_NAME IS NOT NULL",
        ),
        [row(&[
            Some("def"),
            Some("reports"),
            Some("fk_user"),
            Some("def"),
            Some("reports"),
            Some("posts"),
            Some("user_id"),
            Some("1"),
            Some("1"),
            Some("reports"),
            Some("users"),
            Some("id"),
        ])]
    );
    // MySQL answers InnoDB's estimate for CARDINALITY, which it caches and
    // which was 0 on the tables here until it looked again; the engine keeps
    // none, and answers NULL as `SHOW INDEX` does. Every key column is NOT
    // NULL, the counted key among them.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME, INDEX_NAME",
        ),
        [
            row(&[
                Some("def"),
                Some("reports"),
                Some("posts"),
                Some("1"),
                Some("reports"),
                Some("fk_user"),
                Some("1"),
                Some("user_id"),
                Some("A"),
                None,
                None,
                None,
                Some(""),
                Some("BTREE"),
                Some(""),
                Some(""),
                Some("YES"),
                None,
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("posts"),
                Some("0"),
                Some("reports"),
                Some("PRIMARY"),
                Some("1"),
                Some("id"),
                Some("A"),
                None,
                None,
                None,
                Some(""),
                Some("BTREE"),
                Some(""),
                Some(""),
                Some("YES"),
                None,
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("users"),
                Some("0"),
                Some("reports"),
                Some("PRIMARY"),
                Some("1"),
                Some("id"),
                Some("A"),
                None,
                None,
                None,
                Some(""),
                Some("BTREE"),
                Some(""),
                Some(""),
                Some("YES"),
                None,
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("users"),
                Some("0"),
                Some("reports"),
                Some("uk_email"),
                Some("1"),
                Some("email"),
                Some("A"),
                None,
                None,
                None,
                Some(""),
                Some("BTREE"),
                Some(""),
                Some(""),
                Some("YES"),
                None,
            ]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = DATABASE()",
        ),
        [row(&[
            Some("def"),
            Some("reports"),
            Some("fk_user"),
            Some("def"),
            Some("reports"),
            Some("PRIMARY"),
            Some("NONE"),
            Some("NO ACTION"),
            Some("CASCADE"),
            Some("posts"),
            Some("users"),
        ])]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME, CONSTRAINT_NAME",
        ),
        [
            // Measured: a name is ordered without regard to case.
            row(&[
                Some("def"),
                Some("reports"),
                Some("fk_user"),
                Some("reports"),
                Some("posts"),
                Some("FOREIGN KEY"),
                Some("YES"),
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("PRIMARY"),
                Some("reports"),
                Some("posts"),
                Some("PRIMARY KEY"),
                Some("YES"),
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("PRIMARY"),
                Some("reports"),
                Some("users"),
                Some("PRIMARY KEY"),
                Some("YES"),
            ]),
            row(&[
                Some("def"),
                Some("reports"),
                Some("uk_email"),
                Some("reports"),
                Some("users"),
                Some("UNIQUE"),
                Some("YES"),
            ]),
        ]
    );
    // Stored programs are refused here, so no database holds a routine.
    assert!(rows(
        &mut adapter,
        "SELECT * FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE()",
    )
    .is_empty());
}

/// Measured on MySQL 8.4.11 over one of each: a `CHAR`, `VARCHAR`, `ENUM` or
/// `SET` holds four bytes a character, a `TEXT` or a binary type is measured
/// in bytes already, only a `TIME`, `DATETIME` or `TIMESTAMP` has a
/// `DATETIME_PRECISION`, and only a column holding words has a character set.
#[test]
fn the_columns_mysql_adds_answer_what_mysql_measures() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query(
            "CREATE TABLE kinds (id INT NOT NULL PRIMARY KEY, c CHAR(4), vc VARCHAR(8), \
             vb VARBINARY(8), t TEXT, lt LONGTEXT, b BLOB, d DATE, tm TIME, \
             dt DATETIME(3), ts TIMESTAMP NULL, n DECIMAL(8,2), e ENUM('a','b'), \
             s SET('a','b'), u BIGINT UNSIGNED, j JSON, cb VARCHAR(10) COLLATE utf8mb4_bin)",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COLUMN_NAME, CHARACTER_MAXIMUM_LENGTH, CHARACTER_OCTET_LENGTH, \
             NUMERIC_PRECISION, NUMERIC_SCALE, DATETIME_PRECISION, CHARACTER_SET_NAME, \
             COLLATION_NAME, GENERATION_EXPRESSION, SRS_ID FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'kinds' ORDER BY ORDINAL_POSITION",
        ),
        [
            row(&[
                Some("id"),
                None,
                None,
                Some("10"),
                Some("0"),
                None,
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("c"),
                Some("4"),
                Some("16"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                Some(""),
                None,
            ]),
            row(&[
                Some("vc"),
                Some("8"),
                Some("32"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                Some(""),
                None,
            ]),
            row(&[
                Some("vb"),
                Some("8"),
                Some("8"),
                None,
                None,
                None,
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("t"),
                Some("65535"),
                Some("65535"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                Some(""),
                None,
            ]),
            row(&[
                Some("lt"),
                Some("4294967295"),
                Some("4294967295"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                Some(""),
                None,
            ]),
            row(&[
                Some("b"),
                Some("65535"),
                Some("65535"),
                None,
                None,
                None,
                None,
                None,
                Some(""),
                None,
            ]),
            row(&[
                Some("d"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("tm"),
                None,
                None,
                None,
                None,
                Some("0"),
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("dt"),
                None,
                None,
                None,
                None,
                Some("3"),
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("ts"),
                None,
                None,
                None,
                None,
                Some("0"),
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("n"),
                None,
                None,
                Some("8"),
                Some("2"),
                None,
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("e"),
                Some("1"),
                Some("4"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                Some(""),
                None,
            ]),
            row(&[
                Some("s"),
                Some("3"),
                Some("12"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                Some(""),
                None,
            ]),
            row(&[
                Some("u"),
                None,
                None,
                Some("20"),
                Some("0"),
                None,
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("j"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(""),
                None
            ]),
            row(&[
                Some("cb"),
                Some("10"),
                Some("40"),
                None,
                None,
                None,
                Some("utf8mb4"),
                Some("utf8mb4_bin"),
                Some(""),
                None,
            ]),
        ]
    );
}

/// The shapes MySQL reports for the columns only a wildcard used to reach,
/// measured through `SELECT *` with an `ORDER BY`, the reading every other
/// `information_schema` column here is pinned to.
#[test]
fn the_columns_mysql_adds_report_mysqls_shapes() {
    let (_directory, mut adapter) = adapter();
    let columns = read(
        &mut adapter,
        "SELECT * FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users'",
    )
    .columns;
    let shape = |name: &str| {
        let column = columns
            .iter()
            .find(|column| column.name == name)
            .unwrap_or_else(|| panic!("{name} is answered"));
        (
            column.column_type,
            column.column_length,
            column.flags,
            column.original_table.clone(),
        )
    };
    assert_eq!(
        shape("TABLE_CATALOG"),
        (
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            "catalogs".to_owned()
        )
    );
    assert_eq!(
        shape("CHARACTER_OCTET_LENGTH"),
        (MYSQL_TYPE_LONGLONG, 21, MYSQL_NUM_FLAG, String::new())
    );
    assert_eq!(
        shape("DATETIME_PRECISION"),
        (
            MYSQL_TYPE_LONG,
            10,
            MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
            "columns".to_owned()
        )
    );
    assert_eq!(
        shape("PRIVILEGES"),
        (MYSQL_TYPE_VAR_STRING, 616, 0, String::new())
    );
    assert_eq!(
        shape("GENERATION_EXPRESSION"),
        (
            MYSQL_TYPE_BLOB,
            u32::MAX,
            MYSQL_NOT_NULL_FLAG | MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
            String::new()
        )
    );
    assert_eq!(
        shape("SRS_ID"),
        (
            MYSQL_TYPE_LONG,
            10,
            MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
            "columns".to_owned()
        )
    );

    let statistics = read(
        &mut adapter,
        "SELECT CARDINALITY FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = DATABASE()",
    );
    assert_eq!(statistics.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(statistics.columns[0].column_length, 21);
    assert_eq!(statistics.columns[0].flags & MYSQL_NOT_NULL_FLAG, 0);

    let schemata = read(&mut adapter, "SELECT * FROM information_schema.SCHEMATA");
    assert_eq!(schemata.columns[4].column_type, MYSQL_TYPE_NULL);
    assert_eq!(schemata.columns[5].column_type, MYSQL_TYPE_STRING);
    assert_eq!(schemata.columns[5].column_length, 12);
}

/// TypeORM's `hasDatabase`, `hasTable`, `hasColumn` and the collation read in
/// `loadTables`, each as its current release writes it with the values the
/// driver quotes into the text.
#[test]
fn typeorm_reads_the_catalog_as_it_writes_it() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM `INFORMATION_SCHEMA`.`SCHEMATA` WHERE `SCHEMA_NAME` = 'reports'",
        ),
        [row(&[
            Some("def"),
            Some("reports"),
            Some("utf8mb4"),
            Some("utf8mb4_0900_ai_ci"),
            None,
            Some("NO"),
        ])]
    );
    assert!(rows(
        &mut adapter,
        "SELECT * FROM `INFORMATION_SCHEMA`.`SCHEMATA` WHERE `SCHEMA_NAME` = 'elsewhere'",
    )
    .is_empty());
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM `INFORMATION_SCHEMA`.`COLUMNS` WHERE `TABLE_SCHEMA` = 'reports' AND `TABLE_NAME` = 'posts'",
        )
        .len(),
        3
    );
    assert!(rows(
        &mut adapter,
        "SELECT * FROM `INFORMATION_SCHEMA`.`COLUMNS` WHERE `TABLE_SCHEMA` = 'reports' AND `TABLE_NAME` = 'missing'",
    )
    .is_empty());
    let column = rows(
        &mut adapter,
        "SELECT * FROM `INFORMATION_SCHEMA`.`COLUMNS` WHERE `TABLE_SCHEMA` = 'reports' AND `TABLE_NAME` = 'posts' AND `COLUMN_NAME` = 'title'",
    );
    assert_eq!(column.len(), 1);
    assert_eq!(column[0][15].as_deref(), Some("varchar(100)"));
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `SCHEMA_NAME`, `DEFAULT_CHARACTER_SET_NAME` as `CHARSET`, `DEFAULT_COLLATION_NAME` AS `COLLATION` FROM `INFORMATION_SCHEMA`.`SCHEMATA`",
        ),
        [row(&[
            Some("reports"),
            Some("utf8mb4"),
            Some("utf8mb4_0900_ai_ci")
        ])]
    );
}

/// Sequelize's `describeTable`, `showAllTables`, `getForeignKeyReferencesForTable`
/// and the constraint read `removeConstraint` makes, each as v6 writes it,
/// the trailing semicolon included.
#[test]
fn sequelize_reads_the_catalog_as_it_writes_it() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(&mut adapter, "SHOW FULL COLUMNS FROM `posts`;").len(),
        3
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_TYPE = 'BASE TABLE' AND TABLE_SCHEMA = 'reports';",
        ),
        [
            row(&[Some("posts")]),
            row(&[Some("records")]),
            row(&[Some("users")])
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT CONSTRAINT_NAME as constraint_name,CONSTRAINT_NAME as constraintName,CONSTRAINT_SCHEMA as constraintSchema,CONSTRAINT_SCHEMA as constraintCatalog,TABLE_NAME as tableName,TABLE_SCHEMA as tableSchema,TABLE_SCHEMA as tableCatalog,COLUMN_NAME as columnName,REFERENCED_TABLE_SCHEMA as referencedTableSchema,REFERENCED_TABLE_SCHEMA as referencedTableCatalog,REFERENCED_TABLE_NAME as referencedTableName,REFERENCED_COLUMN_NAME as referencedColumnName FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE where TABLE_NAME = 'posts' AND CONSTRAINT_NAME!='PRIMARY' AND CONSTRAINT_SCHEMA='reports' AND REFERENCED_TABLE_NAME IS NOT NULL;",
        ),
        [row(&[
            Some("fk_user"),
            Some("fk_user"),
            Some("reports"),
            Some("reports"),
            Some("posts"),
            Some("reports"),
            Some("reports"),
            Some("user_id"),
            Some("reports"),
            Some("reports"),
            Some("users"),
            Some("id"),
        ])]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT CONSTRAINT_CATALOG AS constraintCatalog, CONSTRAINT_NAME AS constraintName, CONSTRAINT_SCHEMA AS constraintSchema, CONSTRAINT_TYPE AS constraintType, TABLE_NAME AS tableName, TABLE_SCHEMA AS tableSchema from INFORMATION_SCHEMA.TABLE_CONSTRAINTS WHERE table_name='posts' AND constraint_name = 'fk_user' AND TABLE_SCHEMA = 'reports';",
        ),
        [row(&[
            Some("def"),
            Some("fk_user"),
            Some("reports"),
            Some("FOREIGN KEY"),
            Some("posts"),
            Some("reports"),
        ])]
    );
}

/// Doctrine DBAL's `MySQLSchemaManager` reads of one table's columns, indexes
/// and foreign keys, as 4.x writes them with the values bound.
#[test]
fn doctrine_reads_the_catalog_as_it_writes_it() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT
       c.TABLE_NAME,
       c.COLUMN_NAME        AS field,
       c.DATA_TYPE          AS type,
       c.COLUMN_TYPE,
       c.CHARACTER_MAXIMUM_LENGTH,
       c.CHARACTER_OCTET_LENGTH,
       c.NUMERIC_PRECISION,
       c.NUMERIC_SCALE,
       c.IS_NULLABLE        AS `null`,
       c.COLUMN_KEY         AS `key`,
       c.COLUMN_DEFAULT     AS `default`,
       c.EXTRA,
       c.COLUMN_COMMENT     AS comment,
       c.CHARACTER_SET_NAME AS characterset,
       c.COLLATION_NAME     AS collation
FROM information_schema.COLUMNS c
    INNER JOIN information_schema.TABLES t
        ON t.TABLE_NAME = c.TABLE_NAME
 WHERE c.TABLE_SCHEMA = 'reports' AND t.TABLE_SCHEMA = 'reports' AND t.TABLE_NAME = 'users'
   AND t.TABLE_TYPE = 'BASE TABLE'
ORDER BY c.TABLE_NAME,
         c.ORDINAL_POSITION",
        ),
        [
            row(&[
                Some("users"),
                Some("id"),
                Some("int"),
                Some("int"),
                None,
                None,
                Some("10"),
                Some("0"),
                Some("NO"),
                Some("PRI"),
                None,
                Some("auto_increment"),
                Some(""),
                None,
                None,
            ]),
            row(&[
                Some("users"),
                Some("email"),
                Some("varchar"),
                Some("varchar(255)"),
                Some("255"),
                Some("1020"),
                None,
                None,
                Some("NO"),
                Some("UNI"),
                None,
                Some(""),
                Some(""),
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
            ]),
            row(&[
                Some("users"),
                Some("age"),
                Some("int"),
                Some("int"),
                None,
                None,
                Some("10"),
                Some("0"),
                Some("YES"),
                Some(""),
                None,
                Some(""),
                Some(""),
                None,
                None,
            ]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT
        TABLE_NAME,
        NON_UNIQUE  AS Non_Unique,
        INDEX_NAME  AS Key_name,
        COLUMN_NAME AS Column_Name,
        SUB_PART    AS Sub_Part,
        INDEX_TYPE  AS Index_Type
FROM information_schema.STATISTICS
WHERE TABLE_SCHEMA = 'reports' AND TABLE_NAME = 'users'
ORDER BY TABLE_NAME,
         INDEX_NAME,
         SEQ_IN_INDEX",
        ),
        [
            row(&[
                Some("users"),
                Some("0"),
                Some("PRIMARY"),
                Some("id"),
                None,
                Some("BTREE")
            ]),
            row(&[
                Some("users"),
                Some("0"),
                Some("uk_email"),
                Some("email"),
                None,
                Some("BTREE")
            ]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT
            k.TABLE_NAME,
            k.CONSTRAINT_NAME,
            k.COLUMN_NAME,
            k.REFERENCED_TABLE_NAME,
            k.REFERENCED_COLUMN_NAME,
            k.ORDINAL_POSITION,
            c.UPDATE_RULE,
            c.DELETE_RULE
FROM information_schema.key_column_usage k
INNER JOIN information_schema.referential_constraints c
ON c.CONSTRAINT_NAME = k.CONSTRAINT_NAME
AND c.TABLE_NAME = k.TABLE_NAME
WHERE k.TABLE_SCHEMA = 'reports' AND c.CONSTRAINT_SCHEMA = 'reports' AND k.TABLE_NAME = 'posts'
AND k.REFERENCED_COLUMN_NAME IS NOT NULL
ORDER BY k.TABLE_NAME,
         k.CONSTRAINT_NAME,
         k.ORDINAL_POSITION",
        ),
        [row(&[
            Some("posts"),
            Some("fk_user"),
            Some("user_id"),
            Some("users"),
            Some("id"),
            Some("1"),
            Some("NO ACTION"),
            Some("CASCADE"),
        ])]
    );
}

/// TypeORM's `loadTables` for three tables, every read as its current release
/// writes it: a `UNION` with one branch per table, derived tables over
/// wildcards, and joins between those.
///
/// Measured on MySQL 8.4.11 over the same tables. A `UNION` answers its rows
/// in no promised order, so they are compared sorted.
#[test]
fn typeorm_loads_three_tables_as_it_writes_it() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query(
            "CREATE TABLE tags (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
             name VARCHAR(20) NOT NULL, post_id INT, KEY idx_name (name), \
             CONSTRAINT fk_tag_post FOREIGN KEY (post_id) REFERENCES posts (id))",
        )
        .unwrap();
    let per_table = |select: &str, schema_column: &str, table_column: &str| {
        ["users", "posts", "tags"]
            .iter()
            .map(|table| {
                format!("{select} WHERE {schema_column} = 'reports' AND {table_column} = '{table}'")
            })
            .collect::<Vec<_>>()
            .join(" UNION ")
    };
    let sorted = |adapter: &mut Adapter, sql: &str| {
        let mut read = rows(adapter, sql);
        read.sort();
        read
    };

    let tables = per_table(
        "SELECT `TABLE_SCHEMA`, `TABLE_NAME`, `TABLE_COMMENT` FROM `INFORMATION_SCHEMA`.`TABLES`",
        "`TABLE_SCHEMA`",
        "`TABLE_NAME`",
    );
    assert_eq!(
        sorted(&mut adapter, &tables),
        [
            row(&[Some("reports"), Some("posts"), Some("")]),
            row(&[Some("reports"), Some("tags"), Some("")]),
            row(&[Some("reports"), Some("users"), Some("")]),
        ]
    );

    let columns = per_table(
        "SELECT * FROM `INFORMATION_SCHEMA`.`COLUMNS`",
        "`TABLE_SCHEMA`",
        "`TABLE_NAME`",
    );
    let keys = sorted(&mut adapter, &columns)
        .into_iter()
        .map(|column| (column[2].clone(), column[3].clone(), column[16].clone()))
        .collect::<Vec<_>>();
    let key = |table: &str, column: &str, key: &str| {
        (
            Some(table.to_owned()),
            Some(column.to_owned()),
            Some(key.to_owned()),
        )
    };
    assert_eq!(
        keys,
        [
            key("posts", "id", "PRI"),
            key("posts", "title", ""),
            key("posts", "user_id", "MUL"),
            key("tags", "id", "PRI"),
            key("tags", "name", "MUL"),
            key("tags", "post_id", "MUL"),
            key("users", "age", ""),
            key("users", "email", "UNI"),
            key("users", "id", "PRI"),
        ]
    );

    let key_columns = per_table(
        "SELECT * FROM `INFORMATION_SCHEMA`.`KEY_COLUMN_USAGE` `kcu`",
        "`kcu`.`TABLE_SCHEMA`",
        "`kcu`.`TABLE_NAME`",
    );
    let primary_keys = sorted(
        &mut adapter,
        &format!("SELECT * FROM ({key_columns}) `kcu` WHERE `CONSTRAINT_NAME` = 'PRIMARY'"),
    );
    assert_eq!(
        primary_keys
            .iter()
            .map(|key| (key[5].as_deref(), key[6].as_deref()))
            .collect::<Vec<_>>(),
        [
            (Some("posts"), Some("id")),
            (Some("tags"), Some("id")),
            (Some("users"), Some("id")),
        ]
    );
    assert!(primary_keys.iter().all(|key| key.len() == 12));

    let statistics = per_table(
        "SELECT * FROM `INFORMATION_SCHEMA`.`STATISTICS`",
        "`TABLE_SCHEMA`",
        "`TABLE_NAME`",
    );
    let referential = per_table(
        "SELECT * FROM `INFORMATION_SCHEMA`.`REFERENTIAL_CONSTRAINTS`",
        "`CONSTRAINT_SCHEMA`",
        "`TABLE_NAME`",
    );
    let indices = sorted(
        &mut adapter,
        &format!(
            "SELECT `s`.* FROM ({statistics}) `s` LEFT JOIN ({referential}) `rc` ON `s`.`INDEX_NAME` = `rc`.`CONSTRAINT_NAME` AND `s`.`TABLE_SCHEMA` = `rc`.`CONSTRAINT_SCHEMA` WHERE `s`.`INDEX_NAME` != 'PRIMARY' AND `rc`.`CONSTRAINT_NAME` IS NULL"
        ),
    );
    assert_eq!(
        indices
            .iter()
            .map(|index| (
                index[2].as_deref(),
                index[5].as_deref(),
                index[7].as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            (Some("tags"), Some("idx_name"), Some("name")),
            (Some("users"), Some("uk_email"), Some("email")),
        ]
    );
    assert!(indices.iter().all(|index| index.len() == 18));

    let foreign_keys = sorted(
        &mut adapter,
        &format!(
            "SELECT `kcu`.`TABLE_SCHEMA`, `kcu`.`TABLE_NAME`, `kcu`.`CONSTRAINT_NAME`, `kcu`.`COLUMN_NAME`, `kcu`.`REFERENCED_TABLE_SCHEMA`, `kcu`.`REFERENCED_TABLE_NAME`, `kcu`.`REFERENCED_COLUMN_NAME`, `rc`.`DELETE_RULE` `ON_DELETE`, `rc`.`UPDATE_RULE` `ON_UPDATE` FROM ({key_columns}) `kcu` INNER JOIN ({referential}) `rc` ON `rc`.`CONSTRAINT_SCHEMA` = `kcu`.`CONSTRAINT_SCHEMA` AND `rc`.`TABLE_NAME` = `kcu`.`TABLE_NAME` AND `rc`.`CONSTRAINT_NAME` = `kcu`.`CONSTRAINT_NAME`"
        ),
    );
    assert_eq!(
        foreign_keys,
        [
            row(&[
                Some("reports"),
                Some("posts"),
                Some("fk_user"),
                Some("user_id"),
                Some("reports"),
                Some("users"),
                Some("id"),
                Some("CASCADE"),
                Some("NO ACTION"),
            ]),
            row(&[
                Some("reports"),
                Some("tags"),
                Some("fk_tag_post"),
                Some("post_id"),
                Some("reports"),
                Some("posts"),
                Some("id"),
                Some("NO ACTION"),
                Some("NO ACTION"),
            ]),
        ]
    );
}

/// A `UNION` of three is taken only where every branch reads the same columns
/// of the same `information_schema` table: which shape MySQL answers for a
/// column three kinds meet in has not been measured, and MySQL lets a `UNION
/// DISTINCT` undo the `UNION ALL` left of it, which a flat list would lose.
#[test]
fn a_union_of_three_is_taken_only_over_one_catalog_table() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT id FROM users UNION SELECT id FROM posts UNION SELECT id FROM users",
        "SELECT TABLE_NAME FROM information_schema.TABLES UNION ALL SELECT TABLE_NAME FROM information_schema.TABLES UNION SELECT TABLE_NAME FROM information_schema.TABLES",
        "SELECT TABLE_NAME FROM information_schema.TABLES UNION SELECT TABLE_NAME FROM information_schema.STATISTICS UNION SELECT TABLE_NAME FROM information_schema.TABLES",
        "SELECT TABLE_NAME FROM information_schema.TABLES UNION SELECT TABLE_SCHEMA FROM information_schema.TABLES UNION SELECT TABLE_NAME FROM information_schema.TABLES",
        "SELECT * FROM (SELECT * FROM information_schema.TABLES UNION SELECT * FROM information_schema.TABLES) t",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// MySQL holds every key column NOT NULL, and says so for the counted key of
/// an `AUTO_INCREMENT` table too, which the engine does not mark. Measured on
/// MySQL 8.4.11: `SHOW INDEX` answers an empty `Null` for it, and
/// `STATISTICS` an empty `NULLABLE`.
#[test]
fn a_counted_key_is_never_null_in_the_index_listings() {
    let (_directory, mut adapter) = adapter();
    let shown = rows(&mut adapter, "SHOW INDEX FROM users");
    assert_eq!(shown[0][2].as_deref(), Some("PRIMARY"));
    assert_eq!(shown[0][9].as_deref(), Some(""));
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT NULLABLE FROM information_schema.STATISTICS \
             WHERE TABLE_NAME = 'users' AND INDEX_NAME = 'PRIMARY'",
        ),
        [row(&[Some("")])]
    );
}
