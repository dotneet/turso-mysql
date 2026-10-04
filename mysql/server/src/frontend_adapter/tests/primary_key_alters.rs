//! `ALTER TABLE` statements that change a table's primary key, the columns it
//! is over, or the column the table counts on: each writes the table again,
//! keeping its rows, indexes, foreign keys, triggers, views and counter.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// A key's column takes another type and the rows come across: `INT` to
/// `BIGINT` (the migration Rails and Laravel write most), to `BIGINT
/// UNSIGNED`, to a word and back, renamed by `CHANGE`. A `MODIFY` that does
/// not say `NOT NULL` leaves the key `NOT NULL`, and the table's other index
/// stays.
#[test]
fn a_key_column_takes_another_type_and_keeps_its_rows() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v VARCHAR(10), KEY kv (v))",
    );
    run(
        &mut adapter,
        "INSERT INTO t VALUES (5, 'c'), (1, 'a'), (2, 'b')",
    );
    for (sql, printed) in [
        ("ALTER TABLE t MODIFY id BIGINT NOT NULL", "bigint"),
        ("ALTER TABLE t MODIFY id BIGINT", "bigint"),
        (
            "ALTER TABLE t MODIFY id BIGINT UNSIGNED NOT NULL",
            "bigint unsigned",
        ),
        (
            "ALTER TABLE t MODIFY id VARCHAR(20) NOT NULL",
            "varchar(20)",
        ),
        ("ALTER TABLE t MODIFY id INT NOT NULL", "int"),
    ] {
        run(&mut adapter, sql);
        assert_eq!(
            created(&mut adapter, "t"),
            format!(
                "CREATE TABLE `t` (\n  `id` {printed} NOT NULL,\n  `v` varchar(10) DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `kv` (`v`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            ),
            "{sql}"
        );
        assert_eq!(
            rows(&mut adapter, "SELECT id, v FROM t ORDER BY id"),
            [["1", "a"], ["2", "b"], ["5", "c"]],
            "{sql}"
        );
    }
    run(&mut adapter, "ALTER TABLE t CHANGE id pk BIGINT NOT NULL");
    assert_eq!(
        rows(&mut adapter, "SHOW INDEX FROM t")
            .iter()
            .map(|row| (row[2].clone(), row[4].clone()))
            .collect::<Vec<_>>(),
        [
            ("PRIMARY".to_owned(), "pk".to_owned()),
            ("kv".to_owned(), "v".to_owned()),
        ]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT pk FROM t WHERE v = 'b'"),
        [["2"]]
    );
    // The key is the table's rowid again, so a row it holds is refused as a
    // duplicate and a row leaving it out is 1364.
    for (sql, refused) in [
        (
            "INSERT INTO t VALUES (2, 'x')",
            FrontendErrorKind::ConstraintViolation,
        ),
        (
            "INSERT INTO t (v) VALUES ('x')",
            FrontendErrorKind::MissingRequiredDefault,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    // A default given to the key is what a row leaving it out takes.
    run(&mut adapter, "ALTER TABLE t ALTER COLUMN pk SET DEFAULT 7");
    assert!(created(&mut adapter, "t").contains("`pk` bigint NOT NULL DEFAULT '7',"));
    run(&mut adapter, "INSERT INTO t (v) VALUES ('d')");
    assert_eq!(rows(&mut adapter, "SELECT v FROM t WHERE pk = 7"), [["d"]]);
}

/// A value the key's new type cannot hold refuses the statement and leaves
/// the table as it was: 1264 for a number past the type, 1366 for a word
/// naming no number, 1265 for a word cut short by a narrower word column,
/// 1406 for a number too long for one, and 1062 for two rows the new type
/// makes one.
#[test]
fn a_key_value_its_new_type_cannot_hold_changes_nothing() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "CREATE TABLE n (id INT NOT NULL PRIMARY KEY)");
    run(&mut adapter, "INSERT INTO n VALUES (1), (300), (-3)");
    run(
        &mut adapter,
        "CREATE TABLE w (id VARCHAR(10) NOT NULL PRIMARY KEY)",
    );
    run(&mut adapter, "INSERT INTO w VALUES ('1'), ('abc'), ('22')");
    for (sql, refused) in [
        (
            "ALTER TABLE n MODIFY id TINYINT NOT NULL",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "ALTER TABLE n MODIFY id INT UNSIGNED NOT NULL",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "ALTER TABLE n MODIFY id VARCHAR(1) NOT NULL",
            FrontendErrorKind::DataTooLong,
        ),
        (
            "ALTER TABLE w MODIFY id INT NOT NULL",
            FrontendErrorKind::IncorrectValue,
        ),
        (
            "ALTER TABLE w MODIFY id VARCHAR(1) NOT NULL",
            FrontendErrorKind::NotAMember,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    assert!(created(&mut adapter, "n").contains("`id` int NOT NULL"));
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM n ORDER BY id"),
        [["-3"], ["1"], ["300"]]
    );
    assert!(created(&mut adapter, "w").contains("`id` varchar(10) NOT NULL"));
    run(&mut adapter, "DELETE FROM w WHERE id = 'abc'");
    run(&mut adapter, "ALTER TABLE w MODIFY id INT NOT NULL");
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM w ORDER BY id"),
        [["1"], ["22"]]
    );
}

/// `DROP PRIMARY KEY` and `ADD PRIMARY KEY`, alone or together, over one
/// column or several, each with the error MySQL answers when it refuses.
#[test]
fn a_primary_key_is_dropped_and_added() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT, KEY kv (v))",
    );
    run(&mut adapter, "INSERT INTO t VALUES (3, 1), (1, 2), (2, 3)");
    run(&mut adapter, "ALTER TABLE t DROP PRIMARY KEY");
    assert_eq!(
        created(&mut adapter, "t"),
        "CREATE TABLE `t` (\n  `id` int NOT NULL,\n  `v` int DEFAULT NULL,\n  KEY `kv` (`v`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    // Measured: the rows are copied in the old key's order, which is the
    // order a table with no key reads them back in.
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM t"),
        [["1", "2"], ["2", "3"], ["3", "1"]]
    );
    run(&mut adapter, "INSERT INTO t VALUES (1, 9)");
    for (sql, refused) in [
        (
            "ALTER TABLE t DROP PRIMARY KEY",
            FrontendErrorKind::CantDropKey,
        ),
        (
            "ALTER TABLE t ADD PRIMARY KEY (id)",
            FrontendErrorKind::ConstraintViolation,
        ),
        (
            "ALTER TABLE t ADD PRIMARY KEY (nope)",
            FrontendErrorKind::ForeignKeyColumnMissing,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    run(&mut adapter, "DELETE FROM t WHERE v = 9");
    run(
        &mut adapter,
        "ALTER TABLE t ADD CONSTRAINT named PRIMARY KEY (id)",
    );
    assert_eq!(
        created(&mut adapter, "t"),
        "CREATE TABLE `t` (\n  `id` int NOT NULL,\n  `v` int DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `kv` (`v`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        adapter
            .execute_query("ALTER TABLE t ADD PRIMARY KEY (v)")
            .map(|_| ()),
        Err(FrontendErrorKind::SecondPrimaryKey)
    );
    assert_eq!(
        adapter
            .execute_query("ALTER TABLE t MODIFY id INT NULL")
            .map(|_| ()),
        Err(FrontendErrorKind::KeyColumnMayBeNull)
    );

    // A key over a column holding NULL is 1138; the column it is added over
    // reads back NOT NULL.
    run(&mut adapter, "CREATE TABLE n (id INT, v INT)");
    run(&mut adapter, "INSERT INTO n VALUES (1, 1), (NULL, 2)");
    assert_eq!(
        adapter
            .execute_query("ALTER TABLE n ADD PRIMARY KEY (id)")
            .map(|_| ()),
        Err(FrontendErrorKind::InvalidUseOfNull)
    );
    run(&mut adapter, "DELETE FROM n WHERE id IS NULL");
    run(&mut adapter, "ALTER TABLE n ADD PRIMARY KEY (id)");
    assert!(created(&mut adapter, "n").contains("`id` int NOT NULL,"));

    // A key over two columns, and one exchanged for another in one statement.
    run(&mut adapter, "CREATE TABLE c (a INT, b VARCHAR(5), x INT)");
    run(
        &mut adapter,
        "INSERT INTO c VALUES (1, 'y', 2), (1, 'x', 1)",
    );
    run(&mut adapter, "ALTER TABLE c ADD PRIMARY KEY (a, b)");
    assert_eq!(
        created(&mut adapter, "c"),
        "CREATE TABLE `c` (\n  `a` int NOT NULL,\n  `b` varchar(5) NOT NULL,\n  `x` int DEFAULT NULL,\n  PRIMARY KEY (`a`,`b`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        adapter
            .execute_query("INSERT INTO c VALUES (1, 'x', 3)")
            .map(|_| ()),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    run(
        &mut adapter,
        "ALTER TABLE c DROP PRIMARY KEY, ADD PRIMARY KEY (x)",
    );
    assert_eq!(
        created(&mut adapter, "c"),
        "CREATE TABLE `c` (\n  `a` int NOT NULL,\n  `b` varchar(5) NOT NULL,\n  `x` int NOT NULL,\n  PRIMARY KEY (`x`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        rows(&mut adapter, "SELECT x, b FROM c"),
        [["1", "x"], ["2", "y"]]
    );
}

/// `AUTO_INCREMENT` given to a key and taken off it. A table that starts
/// counting numbers each row whose key is NULL or 0 as it copies the rows,
/// reserving as many numbers as it has rows at the first, and counts on past
/// those and its highest id; one that counted before keeps where its counter
/// stood. The column it is given to must be the key.
#[test]
fn a_key_starts_and_stops_counting() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)",
    );
    run(&mut adapter, "INSERT INTO t VALUES (5, 1), (0, 2), (9, 3)");
    run(
        &mut adapter,
        "ALTER TABLE t MODIFY id INT NOT NULL AUTO_INCREMENT",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM t"),
        [["1", "2"], ["5", "1"], ["9", "3"]]
    );
    assert_eq!(counter(&mut adapter, "t").as_deref(), Some("10"));
    assert_eq!(
        written(&mut adapter, "INSERT INTO t (v) VALUES (4)"),
        (1, 10)
    );

    run(&mut adapter, "ALTER TABLE t MODIFY id INT NOT NULL");
    assert_eq!(counter(&mut adapter, "t"), None);
    assert_eq!(
        adapter
            .execute_query("INSERT INTO t (v) VALUES (5)")
            .map(|_| ()),
        Err(FrontendErrorKind::MissingRequiredDefault)
    );
    run(&mut adapter, "INSERT INTO t VALUES (11, 5)");
    // Rails' `change_column :t, :id, :bigint` keeps the column counting.
    run(
        &mut adapter,
        "ALTER TABLE `t` CHANGE `id` `id` bigint NOT NULL AUTO_INCREMENT",
    );
    assert_eq!(counter(&mut adapter, "t").as_deref(), Some("12"));
    run(&mut adapter, "ALTER TABLE t AUTO_INCREMENT = 100");
    run(&mut adapter, "DELETE FROM t WHERE id = 11");
    run(
        &mut adapter,
        "ALTER TABLE t MODIFY id INT NOT NULL AUTO_INCREMENT",
    );
    assert_eq!(counter(&mut adapter, "t").as_deref(), Some("100"));
    assert_eq!(
        written(&mut adapter, "INSERT INTO t (v) VALUES (6)"),
        (1, 100)
    );

    // Measured: rows 5, NULL, 0, NULL, 3, 0 in a table with no key take 6,
    // 7, 8 and 9, and the table reads AUTO_INCREMENT=12.
    run(&mut adapter, "CREATE TABLE n (id INT, v CHAR(1))");
    run(
        &mut adapter,
        "INSERT INTO n VALUES (5, 'a'), (NULL, 'b'), (0, 'c'), (NULL, 'd'), (3, 'e'), (0, 'f')",
    );
    run(
        &mut adapter,
        "ALTER TABLE n MODIFY id INT NOT NULL AUTO_INCREMENT PRIMARY KEY",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM n ORDER BY v"),
        [
            ["5", "a"],
            ["6", "b"],
            ["7", "c"],
            ["8", "d"],
            ["3", "e"],
            ["9", "f"]
        ]
    );
    assert_eq!(counter(&mut adapter, "n").as_deref(), Some("12"));

    // A number the copy hands a row that another row already holds is 1062,
    // and nothing changes.
    run(&mut adapter, "CREATE TABLE d (id INT NOT NULL, v INT)");
    run(&mut adapter, "INSERT INTO d VALUES (0, 1), (1, 2)");
    assert_eq!(
        adapter
            .execute_query("ALTER TABLE d MODIFY id INT NOT NULL AUTO_INCREMENT PRIMARY KEY")
            .map(|_| ()),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM d"),
        [["0", "1"], ["1", "2"]]
    );

    // The counted column has to be the key: 1075 for one no key starts
    // with, and for the key dropped from under it.
    run(
        &mut adapter,
        "CREATE TABLE k (id INT NOT NULL PRIMARY KEY, v INT)",
    );
    run(
        &mut adapter,
        "CREATE TABLE c (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)",
    );
    for sql in [
        "ALTER TABLE k MODIFY v INT AUTO_INCREMENT",
        "ALTER TABLE c DROP PRIMARY KEY",
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(FrontendErrorKind::CountedColumnNotAKey),
            "{sql}"
        );
    }

    // The column a table counts on is renamed with the counter where it
    // stood.
    run(&mut adapter, "INSERT INTO c (v) VALUES (1), (2)");
    run(&mut adapter, "ALTER TABLE c RENAME COLUMN id TO pk");
    assert_eq!(
        created(&mut adapter, "c"),
        "CREATE TABLE `c` (\n  `pk` int NOT NULL AUTO_INCREMENT,\n  `v` int DEFAULT NULL,\n  PRIMARY KEY (`pk`)\n) ENGINE=InnoDB AUTO_INCREMENT=3 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        written(&mut adapter, "INSERT INTO c (v) VALUES (3)"),
        (1, 3)
    );
}

/// A key change MySQL refuses for a foreign key's sake, and one it takes.
/// With `fk` naming `p (id)` from `c (pid)`: the parent's key made `BIGINT`
/// is 3780 whatever `foreign_key_checks` says, and so is the child's column;
/// `AUTO_INCREMENT` given to the parent's key is 1833 with the checks on and
/// taken with them off; given to the child's column it is 1832; the parent's
/// key dropped is 1553, and so is the key of a child found only by it. The
/// parent's key restated as it was is taken, and the key still holds.
#[test]
fn a_foreign_key_holds_a_key_change_to_what_mysql_allows() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE p (id INT NOT NULL PRIMARY KEY, v INT)",
        "CREATE TABLE c (id INT NOT NULL PRIMARY KEY, pid INT, CONSTRAINT fk FOREIGN KEY (pid) REFERENCES p (id))",
        "CREATE TABLE sole (pid INT NOT NULL PRIMARY KEY, CONSTRAINT fk_sole FOREIGN KEY (pid) REFERENCES p (id))",
        "INSERT INTO p VALUES (1, 1), (2, 2)",
        "INSERT INTO c VALUES (10, 1), (11, 2)",
    ] {
        run(&mut adapter, sql);
    }
    for (sql, refused) in [
        (
            "ALTER TABLE p MODIFY id BIGINT NOT NULL",
            FrontendErrorKind::ForeignKeyColumnsIncompatible,
        ),
        (
            "ALTER TABLE c MODIFY pid BIGINT",
            FrontendErrorKind::ForeignKeyColumnsIncompatible,
        ),
        (
            "ALTER TABLE p MODIFY id INT NOT NULL AUTO_INCREMENT",
            FrontendErrorKind::ReferencedColumnCannotChange,
        ),
        (
            "ALTER TABLE c MODIFY pid INT NOT NULL AUTO_INCREMENT",
            FrontendErrorKind::ForeignKeyColumnCannotChange,
        ),
        (
            "ALTER TABLE p DROP PRIMARY KEY",
            FrontendErrorKind::RequiredForeignKeyIndex,
        ),
        (
            "ALTER TABLE sole DROP PRIMARY KEY",
            FrontendErrorKind::RequiredForeignKeyIndex,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    run(&mut adapter, "ALTER TABLE p MODIFY id INT NOT NULL");
    run(&mut adapter, "ALTER TABLE c MODIFY id BIGINT NOT NULL");
    run(&mut adapter, "ALTER TABLE c DROP PRIMARY KEY");
    assert_eq!(
        created(&mut adapter, "c"),
        "CREATE TABLE `c` (\n  `id` bigint NOT NULL,\n  `pid` int DEFAULT NULL,\n  KEY `fk` (`pid`),\n  CONSTRAINT `fk` FOREIGN KEY (`pid`) REFERENCES `p` (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    for (sql, refused) in [
        (
            "INSERT INTO c VALUES (12, 3)",
            FrontendErrorKind::ForeignKeyViolation,
        ),
        (
            "DELETE FROM p WHERE id = 1",
            FrontendErrorKind::ParentRowReferenced,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    run(&mut adapter, "SET foreign_key_checks = 0");
    assert_eq!(
        adapter
            .execute_query("ALTER TABLE p MODIFY id BIGINT NOT NULL")
            .map(|_| ()),
        Err(FrontendErrorKind::ForeignKeyColumnsIncompatible)
    );
    run(
        &mut adapter,
        "ALTER TABLE p MODIFY id INT NOT NULL AUTO_INCREMENT",
    );
    run(&mut adapter, "SET foreign_key_checks = 1");
    assert_eq!(counter(&mut adapter, "p").as_deref(), Some("3"));
    assert_eq!(
        written(&mut adapter, "INSERT INTO p (v) VALUES (3)"),
        (1, 3)
    );
    assert_eq!(
        adapter
            .execute_query("DELETE FROM p WHERE id = 2")
            .map(|_| ()),
        Err(FrontendErrorKind::ParentRowReferenced)
    );

    // The index made for a foreign key goes once a key added over its
    // columns finds the rows instead, and that key cannot then be dropped.
    run(
        &mut adapter,
        "CREATE TABLE fk_only (pid INT NOT NULL, v INT, CONSTRAINT fk9 FOREIGN KEY (pid) REFERENCES p (id))",
    );
    run(&mut adapter, "ALTER TABLE fk_only ADD PRIMARY KEY (pid)");
    assert_eq!(
        created(&mut adapter, "fk_only"),
        "CREATE TABLE `fk_only` (\n  `pid` int NOT NULL,\n  `v` int DEFAULT NULL,\n  PRIMARY KEY (`pid`),\n  CONSTRAINT `fk9` FOREIGN KEY (`pid`) REFERENCES `p` (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        adapter
            .execute_query("ALTER TABLE fk_only DROP PRIMARY KEY")
            .map(|_| ()),
        Err(FrontendErrorKind::RequiredForeignKeyIndex)
    );
}

/// A key column renamed by a change MySQL makes in place is renamed in every
/// foreign key naming it: another table's (its name, its actions and its
/// place among that table's keys kept), the table's own over the column, and
/// one the table points at itself. `information_schema` names the new column
/// and every key is held as before. Beside a change MySQL makes by copying
/// the rows the rename is 1846, after 3780.
#[test]
fn a_renamed_key_column_is_renamed_in_the_foreign_keys_naming_it() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE p (id INT NOT NULL PRIMARY KEY, u INT NOT NULL, name VARCHAR(10), n INT, UNIQUE KEY uk (u))",
        "CREATE TABLE c (cid INT NOT NULL PRIMARY KEY, pid INT, pu INT, x VARCHAR(10), CONSTRAINT fk_c FOREIGN KEY (pid) REFERENCES p (id) ON DELETE CASCADE, FOREIGN KEY (pu) REFERENCES p (u))",
        "CREATE TABLE s (id INT NOT NULL PRIMARY KEY, parent INT, FOREIGN KEY (parent) REFERENCES s (id))",
        "CREATE TABLE p2 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT) AUTO_INCREMENT=50",
        "CREATE TABLE c2 (pid INT, FOREIGN KEY (pid) REFERENCES p2 (id))",
        "INSERT INTO p VALUES (1, 10, 'a', 0), (2, 20, 'b', 0)",
        "INSERT INTO c VALUES (100, 1, NULL, 'x'), (200, 2, 20, 'y')",
        "INSERT INTO s VALUES (1, NULL), (2, 1)",
        "INSERT INTO p2 (v) VALUES (1), (2)",
        "INSERT INTO c2 VALUES (50)",
    ] {
        run(&mut adapter, sql);
    }
    for (sql, refused) in [
        (
            "ALTER TABLE p CHANGE id pk INT NOT NULL AUTO_INCREMENT",
            FrontendErrorKind::ForeignKeyColumnRenamedInACopy,
        ),
        (
            "ALTER TABLE p CHANGE id pk INT NOT NULL, MODIFY n BIGINT",
            FrontendErrorKind::ForeignKeyColumnRenamedInACopy,
        ),
        (
            "ALTER TABLE p CHANGE id pk INT NOT NULL, MODIFY name VARCHAR(70)",
            FrontendErrorKind::ForeignKeyColumnRenamedInACopy,
        ),
        (
            "ALTER TABLE p CHANGE u u2 INT NOT NULL, MODIFY id BIGINT NOT NULL",
            FrontendErrorKind::ForeignKeyColumnsIncompatible,
        ),
        (
            "ALTER TABLE c CHANGE pid parent INT, MODIFY x VARCHAR(5)",
            FrontendErrorKind::ForeignKeyColumnRenamedInACopy,
        ),
        (
            "ALTER TABLE p2 CHANGE id pk INT NOT NULL",
            FrontendErrorKind::ForeignKeyColumnRenamedInACopy,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    run(
        &mut adapter,
        "ALTER TABLE p CHANGE id pk INT NOT NULL, MODIFY name VARCHAR(20)",
    );
    assert_eq!(
        created(&mut adapter, "c"),
        "CREATE TABLE `c` (\n  `cid` int NOT NULL,\n  `pid` int DEFAULT NULL,\n  `pu` int DEFAULT NULL,\n  `x` varchar(10) DEFAULT NULL,\n  PRIMARY KEY (`cid`),\n  KEY `fk_c` (`pid`),\n  KEY `pu` (`pu`),\n  CONSTRAINT `c_ibfk_1` FOREIGN KEY (`pu`) REFERENCES `p` (`u`),\n  CONSTRAINT `fk_c` FOREIGN KEY (`pid`) REFERENCES `p` (`pk`) ON DELETE CASCADE\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = 'probe' AND TABLE_NAME = 'c' AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY CONSTRAINT_NAME"
        ),
        [["c_ibfk_1", "pu", "p", "u"], ["fk_c", "pid", "p", "pk"]]
    );
    assert_eq!(
        adapter
            .execute_query("INSERT INTO c VALUES (300, 3, 10, 'z')")
            .map(|_| ()),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );
    run(&mut adapter, "DELETE FROM p WHERE pk = 1");
    assert_eq!(rows(&mut adapter, "SELECT cid FROM c"), [["200"]]);

    // The table's own key over a renamed column, and a key naming the table
    // itself, follow the column.
    run(&mut adapter, "ALTER TABLE c CHANGE pid parent INT");
    assert!(created(&mut adapter, "c").contains(
        "CONSTRAINT `fk_c` FOREIGN KEY (`parent`) REFERENCES `p` (`pk`) ON DELETE CASCADE"
    ));
    assert_eq!(
        adapter
            .execute_query("INSERT INTO c VALUES (300, 3, 20, 'z')")
            .map(|_| ()),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );
    run(&mut adapter, "ALTER TABLE s CHANGE id sid INT NOT NULL");
    assert!(created(&mut adapter, "s")
        .contains("CONSTRAINT `s_ibfk_1` FOREIGN KEY (`parent`) REFERENCES `s` (`sid`)"));
    assert_eq!(
        adapter
            .execute_query("INSERT INTO s VALUES (3, 9)")
            .map(|_| ()),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );

    // A counted key renamed alone keeps counting where it stood.
    run(&mut adapter, "ALTER TABLE p2 RENAME COLUMN id TO pk");
    assert!(created(&mut adapter, "c2")
        .contains("CONSTRAINT `c2_ibfk_1` FOREIGN KEY (`pid`) REFERENCES `p2` (`pk`)"));
    assert_eq!(counter(&mut adapter, "p2").as_deref(), Some("52"));
    assert_eq!(
        written(&mut adapter, "INSERT INTO p2 (v) VALUES (3)"),
        (1, 52)
    );
    run(
        &mut adapter,
        "ALTER TABLE p2 CHANGE pk id2 INT NOT NULL AUTO_INCREMENT",
    );
    assert!(created(&mut adapter, "c2").contains("REFERENCES `p2` (`id2`)"));
    assert_eq!(
        adapter
            .execute_query("DELETE FROM p2 WHERE id2 = 50")
            .map(|_| ()),
        Err(FrontendErrorKind::ParentRowReferenced)
    );
}

/// The table's own triggers, another table's trigger writing it and a view
/// reading it all stand after the key changes, as MySQL leaves them: each
/// trigger reads back as it was made and fires as before.
#[test]
fn triggers_and_views_stand_through_a_key_change() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)",
        "CREATE TABLE audit (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, tid BIGINT)",
        "CREATE TABLE other (id INT NOT NULL PRIMARY KEY)",
        "CREATE TRIGGER t_ai AFTER INSERT ON t FOR EACH ROW INSERT INTO audit (tid) VALUES (NEW.id)",
        "CREATE TRIGGER other_ai AFTER INSERT ON other FOR EACH ROW INSERT INTO t (id, v) VALUES (NEW.id + 100, 0)",
        "CREATE VIEW tv AS SELECT id, v FROM t",
        "INSERT INTO t VALUES (1, 1)",
    ] {
        run(&mut adapter, sql);
    }
    let triggers = rows(&mut adapter, "SHOW TRIGGERS");
    let made_as = rows(&mut adapter, "SHOW CREATE TRIGGER t_ai");
    for sql in [
        "ALTER TABLE t MODIFY id BIGINT NOT NULL",
        "ALTER TABLE t MODIFY id BIGINT NOT NULL AUTO_INCREMENT",
        "ALTER TABLE t DROP PRIMARY KEY, MODIFY id BIGINT NOT NULL",
        "ALTER TABLE t ADD PRIMARY KEY (id)",
    ] {
        run(&mut adapter, sql);
        assert_eq!(rows(&mut adapter, "SHOW TRIGGERS"), triggers, "{sql}");
        assert_eq!(
            rows(&mut adapter, "SHOW CREATE TRIGGER t_ai"),
            made_as,
            "{sql}"
        );
    }
    run(&mut adapter, "INSERT INTO t VALUES (2, 2)");
    run(&mut adapter, "INSERT INTO other VALUES (5)");
    assert_eq!(
        rows(&mut adapter, "SELECT tid FROM audit ORDER BY id"),
        [["1"], ["2"], ["105"]]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM tv ORDER BY id"),
        [["1", "1"], ["2", "2"], ["105", "0"]]
    );
    assert_eq!(
        rows(&mut adapter, "SHOW COLUMNS FROM tv")[0][..3],
        ["id", "bigint", "NO"]
    );
}

/// `ALTER TABLE t ENGINE=InnoDB`, `ALTER TABLE t FORCE` and `OPTIMIZE TABLE t`
/// each make the table again with its rows, indexes, triggers and counter as
/// they were, committing what came before. `OPTIMIZE TABLE` answers a note
/// and a status for each table it names, and an error row for a name that is
/// not there or is a view's; `FORCE` and `ENGINE=InnoDB` answer 1146 for a
/// name that is not there.
#[test]
fn a_table_is_written_again_as_it_stands() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE ai (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT, KEY kv (v)) AUTO_INCREMENT=50",
        "CREATE TABLE audit (tid INT)",
        "CREATE TRIGGER ai_ai AFTER INSERT ON ai FOR EACH ROW INSERT INTO audit (tid) VALUES (NEW.id)",
        "CREATE VIEW av AS SELECT id, v FROM ai",
        "INSERT INTO ai (v) VALUES (1)",
        "DELETE FROM ai",
    ] {
        run(&mut adapter, sql);
    }
    let table = created(&mut adapter, "ai");
    let note = |table: &str| {
        vec![
            vec![
                format!("probe.{table}"),
                "optimize".to_owned(),
                "note".to_owned(),
                "Table does not support optimize, doing recreate + analyze instead".to_owned(),
            ],
            vec![
                format!("probe.{table}"),
                "optimize".to_owned(),
                "status".to_owned(),
                "OK".to_owned(),
            ],
        ]
    };
    assert_eq!(rows(&mut adapter, "OPTIMIZE TABLE ai"), note("ai"));
    assert_eq!(created(&mut adapter, "ai"), table);
    assert_eq!(counter(&mut adapter, "ai").as_deref(), Some("51"));
    for sql in [
        "ALTER TABLE ai FORCE",
        "ALTER TABLE ai ENGINE=InnoDB",
        "OPTIMIZE NO_WRITE_TO_BINLOG TABLE ai",
        "OPTIMIZE LOCAL TABLE ai",
    ] {
        run(&mut adapter, sql);
        assert_eq!(created(&mut adapter, "ai"), table, "{sql}");
    }
    // The statement commits the transaction it stands in.
    run(&mut adapter, "BEGIN");
    assert_eq!(
        written(&mut adapter, "INSERT INTO ai (v) VALUES (2)"),
        (1, 51)
    );
    assert_eq!(rows(&mut adapter, "OPTIMIZE TABLE ai"), note("ai"));
    run(&mut adapter, "ROLLBACK");
    assert_eq!(rows(&mut adapter, "SELECT id, v FROM av"), [["51", "2"]]);
    assert_eq!(
        rows(&mut adapter, "SELECT tid FROM audit ORDER BY tid"),
        [["50"], ["51"]]
    );
    let mut several = note("ai");
    several.extend([
        vec![
            "probe.nope".to_owned(),
            "optimize".to_owned(),
            "Error".to_owned(),
            "Table 'probe.nope' doesn't exist".to_owned(),
        ],
        vec![
            "probe.nope".to_owned(),
            "optimize".to_owned(),
            "status".to_owned(),
            "Operation failed".to_owned(),
        ],
        vec![
            "probe.av".to_owned(),
            "optimize".to_owned(),
            "Error".to_owned(),
            "'probe.av' is not BASE TABLE".to_owned(),
        ],
        vec![
            "probe.av".to_owned(),
            "optimize".to_owned(),
            "status".to_owned(),
            "Operation failed".to_owned(),
        ],
    ]);
    assert_eq!(rows(&mut adapter, "OPTIMIZE TABLE ai, nope, av"), several);
    for sql in ["ALTER TABLE nope FORCE", "ALTER TABLE nope ENGINE=InnoDB"] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(FrontendErrorKind::MissingObject),
            "{sql}"
        );
    }
}

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("root"));
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("probe").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([167; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("probe").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

/// The affected rows and the id one write reports.
fn written(adapter: &mut Adapter, sql: &str) -> (u64, u64) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => (result.affected_rows, result.last_insert_id),
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<String>> {
    let result = adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    let CommandExecutionResult::ResultSet(result) = result else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| {
                    value.map_or("NULL".to_owned(), |value| String::from_utf8(value).unwrap())
                })
                .collect()
        })
        .collect()
}

fn created(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE `{table}`"))[0][1].clone()
}

fn counter(adapter: &mut Adapter, table: &str) -> Option<String> {
    created(adapter, table)
        .split_whitespace()
        .find_map(|word| word.strip_prefix("AUTO_INCREMENT="))
        .map(str::to_owned)
}
