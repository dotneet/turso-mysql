use super::{
    admin_command::{tokenize_definition_statement, AdminToken},
    NamedTable, SessionSqlMode,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefinitionTargets {
    pub tables: Vec<NamedTable>,
    pub dropped_triggers: Vec<NamedTable>,
}

pub fn tables_a_definition_changes(sql: &str, mode: SessionSqlMode) -> DefinitionTargets {
    let Ok(tokens) = tokenize_definition_statement(sql, mode) else {
        return DefinitionTargets::default();
    };
    let tokens: Vec<&AdminToken> = tokens
        .iter()
        .filter(|token| !matches!(token, AdminToken::Comment))
        .collect();
    let mut reader = Reader {
        tokens: &tokens,
        cursor: 0,
        targets: DefinitionTargets::default(),
    };
    if reader.word("ALTER") {
        reader.altered_table();
    } else if reader.word("CREATE") {
        reader.created_object();
    } else if reader.word("DROP") {
        reader.dropped_objects();
    } else if reader.word("TRUNCATE") {
        reader.word("TABLE");
        reader.name_into_tables();
    } else if reader.word("RENAME") {
        reader.renamed_tables();
    }
    reader.targets
}

struct Reader<'a> {
    tokens: &'a [&'a AdminToken],
    cursor: usize,
    targets: DefinitionTargets,
}

impl Reader<'_> {
    fn altered_table(&mut self) {
        while self.word("ONLINE") || self.word("OFFLINE") || self.word("IGNORE") {}
        if !self.word("TABLE") {
            return;
        }
        self.name_into_tables();
        self.referenced_tables();
        self.cursor = 0;
        while self.skip_to_word("RENAME") {
            let renames_the_table = self.word("TO")
                || self.word("AS")
                || !(self.is_word("COLUMN") || self.is_word("INDEX") || self.is_word("KEY"));
            if renames_the_table {
                self.name_into_tables();
            }
        }
    }

    fn created_object(&mut self) {
        while self.cursor < self.tokens.len() {
            if self.word("TEMPORARY") {
                return;
            }
            if self.word("TABLE") {
                self.skip_if_exists_or_not_exists();
                self.name_into_tables();
                self.referenced_tables();
                return;
            }
            if self.word("VIEW") {
                self.name_into_tables();
                return;
            }
            if self.word("INDEX") || self.word("TRIGGER") {
                if self.skip_to_word("ON") {
                    self.name_into_tables();
                }
                return;
            }
            if self.names_something_else() {
                return;
            }
            self.cursor += 1;
        }
    }

    fn dropped_objects(&mut self) {
        if self.word("TEMPORARY") {
            return;
        }
        if self.word("TABLE") || self.word("TABLES") || self.word("VIEW") {
            self.skip_if_exists_or_not_exists();
            self.names_into_tables();
        } else if self.word("INDEX") {
            if self.skip_to_word("ON") {
                self.name_into_tables();
            }
        } else if self.word("TRIGGER") {
            self.skip_if_exists_or_not_exists();
            if let Some(trigger) = self.name() {
                self.targets.dropped_triggers.push(trigger);
            }
        }
    }

    fn renamed_tables(&mut self) {
        if !self.word("TABLE") && !self.word("TABLES") {
            return;
        }
        loop {
            self.name_into_tables();
            if !self.word("TO") {
                return;
            }
            self.name_into_tables();
            if !matches!(self.tokens.get(self.cursor), Some(AdminToken::Comma)) {
                return;
            }
            self.cursor += 1;
        }
    }

    fn names_something_else(&self) -> bool {
        matches!(self.tokens.get(self.cursor), Some(AdminToken::LeftParen))
            || [
                "DATABASE",
                "SCHEMA",
                "USER",
                "ROLE",
                "PROCEDURE",
                "FUNCTION",
                "EVENT",
                "SERVER",
                "TABLESPACE",
                "LOGFILE",
                "RESOURCE",
            ]
            .iter()
            .any(|word| self.is_word(word))
    }

    fn referenced_tables(&mut self) {
        while self.skip_to_word("REFERENCES") {
            self.name_into_tables();
        }
    }

    fn names_into_tables(&mut self) {
        loop {
            self.name_into_tables();
            if !matches!(self.tokens.get(self.cursor), Some(AdminToken::Comma)) {
                return;
            }
            self.cursor += 1;
        }
    }

    fn name_into_tables(&mut self) {
        if let Some(table) = self.name() {
            self.targets.tables.push(table);
        }
    }

    fn name(&mut self) -> Option<NamedTable> {
        let first = self.identifier()?;
        if !matches!(self.tokens.get(self.cursor), Some(AdminToken::Dot)) {
            return Some(NamedTable {
                database: None,
                table: first,
            });
        }
        self.cursor += 1;
        let table = self.identifier()?;
        Some(NamedTable {
            database: Some(first),
            table,
        })
    }

    fn identifier(&mut self) -> Option<String> {
        let identifier = match self.tokens.get(self.cursor) {
            Some(AdminToken::Word(word)) | Some(AdminToken::QuotedIdentifier(word)) => word.clone(),
            _ => return None,
        };
        self.cursor += 1;
        Some(identifier)
    }

    fn skip_if_exists_or_not_exists(&mut self) {
        let start = self.cursor;
        if self.word("IF") && (self.word("EXISTS") || (self.word("NOT") && self.word("EXISTS"))) {
            return;
        }
        self.cursor = start;
    }

    fn skip_to_word(&mut self, expected: &str) -> bool {
        while self.cursor < self.tokens.len() {
            if self.word(expected) {
                return true;
            }
            self.cursor += 1;
        }
        false
    }

    fn word(&mut self, expected: &str) -> bool {
        if !self.is_word(expected) {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn is_word(&self, expected: &str) -> bool {
        matches!(self.tokens.get(self.cursor), Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case(expected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(sql: &str) -> Vec<NamedTable> {
        tables_a_definition_changes(sql, SessionSqlMode::default()).tables
    }

    fn bare(names: &[&str]) -> Vec<NamedTable> {
        names
            .iter()
            .map(|name| NamedTable {
                database: None,
                table: name.to_string(),
            })
            .collect()
    }

    #[test]
    fn each_definition_statement_names_the_tables_it_changes() {
        let cases: &[(&str, &[&str])] = &[
            ("ALTER TABLE t ADD COLUMN c INT", &["t"]),
            ("ALTER TABLE `t` RENAME COLUMN a TO b", &["t"]),
            ("ALTER TABLE t RENAME TO s", &["t", "s"]),
            (
                "ALTER TABLE c ADD CONSTRAINT f FOREIGN KEY (p) REFERENCES p (id)",
                &["c", "p"],
            ),
            ("CREATE TABLE IF NOT EXISTS d (id INT)", &["d"]),
            (
                "CREATE TABLE c (id INT, p INT, FOREIGN KEY (p) REFERENCES `p`(id))",
                &["c", "p"],
            ),
            ("CREATE UNIQUE INDEX i ON t (v)", &["t"]),
            (
                "CREATE OR REPLACE ALGORITHM=MERGE DEFINER=`a`@`%` SQL SECURITY DEFINER VIEW v AS SELECT 1",
                &["v"],
            ),
            (
                "CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW SET NEW.v = 1",
                &["t"],
            ),
            ("CREATE TEMPORARY TABLE x (id INT)", &[]),
            ("DROP TABLE IF EXISTS a, `b`", &["a", "b"]),
            ("DROP VIEW v", &["v"]),
            ("DROP INDEX i ON t", &["t"]),
            ("DROP TEMPORARY TABLE x", &[]),
            ("TRUNCATE TABLE t", &["t"]),
            ("TRUNCATE t", &["t"]),
            ("RENAME TABLE a TO b, c TO d", &["a", "b", "c", "d"]),
            ("SELECT * FROM t", &[]),
            ("ALTER DATABASE app CHARACTER SET utf8mb4", &[]),
            ("CREATE DATABASE app", &[]),
            (
                "CREATE PROCEDURE p() BEGIN CREATE TABLE x (id INT); END",
                &[],
            ),
            ("CREATE USER 'a'@'%' IDENTIFIED BY 'table'", &[]),
            ("DROP DATABASE app", &[]),
        ];
        for (sql, expected) in cases {
            assert_eq!(tables(sql), bare(expected), "{sql}");
        }
    }

    #[test]
    fn a_name_keeps_the_database_it_was_written_with() {
        assert_eq!(
            tables("ALTER TABLE app.t ADD COLUMN c INT"),
            [NamedTable {
                database: Some("app".to_string()),
                table: "t".to_string(),
            }]
        );
    }

    #[test]
    fn a_dropped_trigger_is_named_apart_from_tables() {
        let targets =
            tables_a_definition_changes("DROP TRIGGER IF EXISTS g", SessionSqlMode::default());
        assert!(targets.tables.is_empty());
        assert_eq!(
            targets.dropped_triggers,
            [NamedTable {
                database: None,
                table: "g".to_string(),
            }]
        );
    }
}
