use super::{parse_optional_drop_view, unsupported, MySqlTableName, ParseError, SessionSqlMode};

/// A schema statement that the MySQL CLI sends after reading a mysqldump file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlDumpDdl {
    normalized_sql: String,
    definer: Option<String>,
}

impl MySqlDumpDdl {
    pub fn normalized_sql(&self) -> &str {
        &self.normalized_sql
    }

    pub fn definer(&self) -> Option<&str> {
        self.definer.as_deref()
    }
}

/// Reads only the executable-comment CREATE forms emitted for views and
/// triggers by mysqldump 8.0.46. Other comments are left to the usual parser.
pub fn parse_optional_mysqldump_ddl(sql: &str) -> Result<Option<MySqlDumpDdl>, ParseError> {
    let sql = sql.trim().trim_end_matches(';').trim();
    if !sql.starts_with("/*!") {
        return Ok(None);
    }
    let expanded = expand_create_comments(sql)?;
    if let Some(rest) = expanded.strip_prefix("CREATE DEFINER=") {
        let (definer, rest) = take_definer(rest)?;
        let Some(rest) = rest.strip_prefix(" TRIGGER ") else {
            return unsupported("mysqldump CREATE DEFINER statement");
        };
        let trigger = normalize_trigger_body(rest)?;
        return Ok(Some(MySqlDumpDdl {
            normalized_sql: format!("CREATE TRIGGER {trigger}"),
            definer: Some(definer),
        }));
    }
    if let Some(rest) = expanded.strip_prefix("CREATE ALGORITHM=UNDEFINED DEFINER=") {
        let (definer, rest) = take_definer(rest)?;
        let Some(rest) = rest.strip_prefix(" SQL SECURITY DEFINER VIEW ") else {
            return unsupported("mysqldump CREATE VIEW security option");
        };
        return Ok(Some(MySqlDumpDdl {
            normalized_sql: format!("CREATE VIEW {rest}"),
            definer: Some(definer),
        }));
    }
    if expanded.starts_with("CREATE VIEW ") {
        return Ok(Some(MySqlDumpDdl {
            normalized_sql: expanded,
            definer: None,
        }));
    }
    unsupported("mysqldump executable CREATE comment")
}

/// `mysqldump` drops its temporary view before creating the final definition.
pub fn parse_optional_mysqldump_drop_view(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlTableName>, ParseError> {
    let sql = sql.trim().trim_end_matches(';').trim();
    let Some(inner) = sql
        .strip_prefix("/*!50001 ")
        .and_then(|sql| sql.strip_suffix("*/"))
    else {
        return Ok(None);
    };
    let Some(name) = inner.strip_prefix("DROP VIEW IF EXISTS ") else {
        return Ok(None);
    };
    parse_optional_drop_view(&format!("DROP VIEW {name}"), mode)?
        .map_or_else(|| unsupported("mysqldump DROP VIEW"), |name| Ok(Some(name)))
}

fn expand_create_comments(mut sql: &str) -> Result<String, ParseError> {
    let mut expanded = String::new();
    while !sql.is_empty() {
        let Some(rest) = sql.strip_prefix("/*!") else {
            return unsupported("mysqldump executable CREATE comment");
        };
        let Some((version, rest)) = rest.split_at_checked(5) else {
            return unsupported("mysqldump executable CREATE version");
        };
        if !version.bytes().all(|byte| byte.is_ascii_digit())
            || !["50001", "50003", "50013", "50017"].contains(&version)
        {
            return unsupported("mysqldump executable CREATE version");
        }
        let Some(end) = rest.find("*/") else {
            return unsupported("mysqldump executable CREATE comment");
        };
        let body = rest[..end].trim();
        if body.contains("/*") || body.contains("*/") || body.is_empty() {
            return unsupported("mysqldump nested executable CREATE comment");
        }
        if !expanded.is_empty() {
            expanded.push(' ');
        }
        expanded.push_str(body);
        sql = rest[end + 2..].trim();
    }
    Ok(expanded)
}

fn take_definer(sql: &str) -> Result<(String, &str), ParseError> {
    let Some((user, rest)) = take_backtick_name(sql) else {
        return unsupported("mysqldump DEFINER user");
    };
    let Some(rest) = rest.strip_prefix('@') else {
        return unsupported("mysqldump DEFINER host");
    };
    let Some((host, rest)) = take_backtick_name(rest) else {
        return unsupported("mysqldump DEFINER host");
    };
    if host != "%" || user.is_empty() {
        return unsupported("mysqldump DEFINER host");
    }
    Ok((user, rest))
}

fn take_backtick_name(sql: &str) -> Option<(String, &str)> {
    let rest = sql.strip_prefix('`')?;
    let mut name = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if character == '`' {
            if chars.peek().is_some_and(|(_, next)| *next == '`') {
                name.push('`');
                chars.next();
                continue;
            }
            return Some((name, &rest[index + 1..]));
        }
        name.push(character);
    }
    None
}

fn normalize_trigger_body(trigger: &str) -> Result<String, ParseError> {
    let Some((head, body)) = trigger.split_once(" FOR EACH ROW ") else {
        return unsupported("mysqldump TRIGGER body");
    };
    if body.starts_with("BEGIN ") && body.ends_with(" END") {
        return Ok(trigger.to_owned());
    }
    if body.contains(';') {
        return unsupported("mysqldump TRIGGER body");
    }
    Ok(format!("{head} FOR EACH ROW BEGIN {body}; END"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_create_statements_mysql_cli_sends_from_a_compact_dump() {
        let trigger = "/*!50003 CREATE*/ /*!50017 DEFINER=`gateadmin`@`%`*/ /*!50003 TRIGGER `copy` AFTER INSERT ON `records` FOR EACH ROW INSERT INTO audit (id) VALUES (NEW.id) */";
        let parsed = parse_optional_mysqldump_ddl(trigger).unwrap().unwrap();
        assert_eq!(parsed.definer(), Some("gateadmin"));
        assert_eq!(parsed.normalized_sql(), "CREATE TRIGGER `copy` AFTER INSERT ON `records` FOR EACH ROW BEGIN INSERT INTO audit (id) VALUES (NEW.id); END");

        let view = "/*!50001 CREATE ALGORITHM=UNDEFINED */\n/*!50013 DEFINER=`gateadmin`@`%` SQL SECURITY DEFINER */\n/*!50001 VIEW `names` AS select `records`.`id` AS `id` from `records` */";
        let parsed = parse_optional_mysqldump_ddl(view).unwrap().unwrap();
        assert_eq!(parsed.definer(), Some("gateadmin"));
        assert_eq!(
            parsed.normalized_sql(),
            "CREATE VIEW `names` AS select `records`.`id` AS `id` from `records`"
        );
        assert!(matches!(
            crate::parse_schema_ddl_ast(trigger, crate::SessionSqlMode::default()),
            Ok(turso_parser::ast::Stmt::CreateTrigger { .. })
        ));
        assert!(matches!(
            crate::parse_schema_ddl_ast(view, crate::SessionSqlMode::default()),
            Ok(turso_parser::ast::Stmt::CreateView { .. })
        ));
    }

    #[test]
    fn refuses_a_different_definer_or_unknown_executable_comment() {
        for sql in [
            "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER t AFTER INSERT ON x FOR EACH ROW INSERT INTO y (id) VALUES (NEW.id) */",
            "/*!99999 CREATE VIEW v AS SELECT 1 */",
            "/*!50001 CREATE VIEW v AS SELECT 1 */ trailing",
        ] {
            assert!(parse_optional_mysqldump_ddl(sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn reads_the_drop_before_the_final_view() {
        let drop = "/*!50001 DROP VIEW IF EXISTS `names`*/;";
        assert_eq!(
            parse_optional_mysqldump_drop_view(drop, crate::SessionSqlMode::default())
                .unwrap()
                .unwrap()
                .as_str(),
            "names"
        );
        assert_eq!(
            parse_optional_mysqldump_drop_view("DROP VIEW names", crate::SessionSqlMode::default())
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_dump_select_can_disable_the_absent_query_cache() {
        assert!(crate::parse_select(
            "SELECT /*!40001 SQL_NO_CACHE */ * FROM records",
            crate::SessionSqlMode::default()
        )
        .is_ok());
        assert!(crate::parse_select(
            "SELECT SQL_CALC_FOUND_ROWS * FROM records",
            crate::SessionSqlMode::default()
        )
        .is_err());
    }
}
