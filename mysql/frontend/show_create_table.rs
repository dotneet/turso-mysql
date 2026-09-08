// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Renders `SHOW CREATE TABLE` output.
//!
//! Every rule here was read off the pinned MySQL 8.4.11 golden bytes: two
//! spaces of indent, `,\n` between items, no trailing newline, lower-case type
//! names, and DEFAULT literals in single quotes even when they are numbers.

use crate::session::{MySqlColumnDefault, MySqlColumnMetadata, MySqlIndexEntry};

/// What MySQL puts after `ENGINE=InnoDB`, past the optional counter.
///
/// Turso is not InnoDB and does not use `utf8mb4_0900_ai_ci`, but MySQL always
/// sends these bytes and clients parse them, so the compatibility surface
/// repeats them verbatim.
const TABLE_TRAILER: &str = " DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

/// Renders the `Create Table` column of `SHOW CREATE TABLE`.
///
/// `next_auto_increment` is the value MySQL prints as `AUTO_INCREMENT=<n>`,
/// which it leaves out entirely while the counter is still at one.
pub fn render_create_table(
    table: &str,
    columns: &[MySqlColumnMetadata],
    indexes: &[MySqlIndexEntry],
    foreign_keys: &[MySqlForeignKey],
    next_auto_increment: Option<u64>,
) -> Option<String> {
    if columns.is_empty() {
        return None;
    }
    let mut items = Vec::with_capacity(columns.len() + 1);
    for column in columns {
        items.push(render_column(column)?);
    }
    items.extend(render_keys(indexes));
    items.extend(
        foreign_keys
            .iter()
            .map(|key| render_foreign_key(table, key)),
    );
    let body = items
        .iter()
        .map(|item| format!("  {item}"))
        .collect::<Vec<_>>()
        .join(",\n");
    let counter = next_auto_increment
        .map(|next| format!(" AUTO_INCREMENT={next}"))
        .unwrap_or_default();
    Some(format!(
        "CREATE TABLE {} (\n{body}\n) ENGINE=InnoDB{counter}{TABLE_TRAILER}",
        quoted(table)
    ))
}

/// One foreign key of a table, as the schema holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlForeignKey {
    /// The name a `CONSTRAINT` clause gave it, or `None` where it was written
    /// without one and MySQL's own naming applies.
    pub name: Option<String>,
    /// Where this key sits among the table's, counted from zero.
    pub declaration_order: usize,
    pub child_columns: Vec<String>,
    pub parent_table: String,
    pub parent_columns: Vec<String>,
    /// `ON DELETE <action>` as MySQL spells it, or `None` for the default.
    pub on_delete: Option<String>,
    /// `ON UPDATE <action>`, the same.
    pub on_update: Option<String>,
}

/// Renders one foreign key the way MySQL prints it.
///
/// Measured on MySQL 8.4.11: a constraint written without a name is printed as
/// `` CONSTRAINT `t_ibfk_1` FOREIGN KEY (`a`) REFERENCES `p` (`id`) ``,
/// numbered from one in declaration order, and one written with a name is
/// printed under the name it was given.
fn render_foreign_key(table: &str, key: &MySqlForeignKey) -> String {
    let columns = |names: &[String]| {
        names
            .iter()
            .map(|name| quoted(name))
            .collect::<Vec<_>>()
            .join(",")
    };
    let name = match &key.name {
        Some(name) => name.clone(),
        None => format!("{table}_ibfk_{}", key.declaration_order + 1),
    };
    let mut rendered = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        quoted(&name),
        columns(&key.child_columns),
        quoted(&key.parent_table),
        columns(&key.parent_columns),
    );
    if let Some(action) = &key.on_delete {
        rendered.push_str(&format!(" ON DELETE {action}"));
    }
    if let Some(action) = &key.on_update {
        rendered.push_str(&format!(" ON UPDATE {action}"));
    }
    rendered
}

/// Renders the key lines, in the order MySQL prints them.
///
/// Measured on MySQL 8.4.11: the primary key first, then the unique keys, then
/// the plain ones, each group in creation order rather than by name, and a
/// multi-column key as `` (`a`,`b`) `` with no space after the comma. The
/// entries arrive already in that order, one per indexed column, so this only
/// has to gather each key's columns without disturbing it.
fn render_keys(indexes: &[MySqlIndexEntry]) -> Vec<String> {
    let mut keys: Vec<(&str, bool, Vec<&str>)> = Vec::new();
    for entry in indexes {
        match keys.last_mut() {
            Some((name, _, columns)) if *name == entry.key_name() => {
                columns.push(entry.column_name());
            }
            _ => keys.push((entry.key_name(), entry.unique(), vec![entry.column_name()])),
        }
    }
    keys.into_iter()
        .map(|(name, unique, columns)| {
            let columns = columns
                .into_iter()
                .map(quoted)
                .collect::<Vec<_>>()
                .join(",");
            if name == "PRIMARY" {
                return format!("PRIMARY KEY ({columns})");
            }
            let kind = if unique { "UNIQUE KEY" } else { "KEY" };
            format!("{kind} {} ({columns})", quoted(name))
        })
        .collect()
}

fn render_column(column: &MySqlColumnMetadata) -> Option<String> {
    let mut rendered = format!("{} {}", quoted(column.name()), type_name(column)?);
    if !column.nullable() {
        rendered.push_str(" NOT NULL");
    } else if column.type_name() == "TIMESTAMP" {
        // Measured: `timestamp NULL DEFAULT NULL`, where a nullable DATETIME is
        // only `datetime DEFAULT NULL`.
        rendered.push_str(" NULL");
    }
    rendered.push_str(&render_default(column)?);
    match column.extra() {
        "" => {}
        "AUTO_INCREMENT" => rendered.push_str(" AUTO_INCREMENT"),
        // Measured: a column defaulting to the moment it is written reports
        // `DEFAULT_GENERATED` to `SHOW COLUMNS` and prints nothing extra here,
        // the `DEFAULT CURRENT_TIMESTAMP` already saying it.
        "DEFAULT_GENERATED" => {}
        // Measured: the words are printed after the DEFAULT clause, and the
        // `DEFAULT_GENERATED` half of the extra is not printed at all.
        "on update CURRENT_TIMESTAMP" | "DEFAULT_GENERATED on update CURRENT_TIMESTAMP" => {
            rendered.push_str(" ON UPDATE CURRENT_TIMESTAMP");
        }
        _ => return None,
    }
    Some(rendered)
}

/// Returns the DEFAULT clause, empty when the column has none, or `None` when
/// the default is one this renderer refuses to print.
///
/// MySQL prints `DEFAULT NULL` for a nullable column that was never given a
/// default, but only when the type can hold one: text and blob columns get no
/// DEFAULT clause at all.
fn render_default(column: &MySqlColumnMetadata) -> Option<String> {
    let Some(default) = column.default_value() else {
        if column.nullable()
            && !matches!(
                column.type_name(),
                "TEXT"
                    | "TINYTEXT"
                    | "MEDIUMTEXT"
                    | "LONGTEXT"
                    | "BLOB"
                    | "TINYBLOB"
                    | "MEDIUMBLOB"
                    | "LONGBLOB"
            )
        {
            return Some(" DEFAULT NULL".to_owned());
        }
        return Some(String::new());
    };
    Some(match default {
        MySqlColumnDefault::Null => " DEFAULT NULL".to_owned(),
        MySqlColumnDefault::Integer { text, .. } => format!(" DEFAULT '{text}'"),
        MySqlColumnDefault::Boolean(value) => {
            format!(" DEFAULT '{}'", u8::from(*value))
        }
        // MySQL prints this one without quotes, it naming a moment rather than
        // holding a value.
        MySqlColumnDefault::Moment => " DEFAULT CURRENT_TIMESTAMP".to_owned(),
        // MySQL escapes a string default the way its own parser reads it back
        // (`\'`, `\n`, `\Z`), and it never lets a string default onto the
        // integer columns this frontend supports in the first place. Refusing
        // is safer than printing DDL whose quoting or line structure differs.
        MySqlColumnDefault::Text(_) => return None,
    })
}

/// Renders the type the way MySQL 8.4.11 prints it here, lower case and
/// carrying the declared length where the type has one.
fn type_name(column: &MySqlColumnMetadata) -> Option<String> {
    if let Some((precision, scale)) = column.decimal_size() {
        return match column.type_name() {
            "DECIMAL" => Some(format!("decimal({precision},{scale})")),
            // Measured on MySQL 8.4.11: the sign prints after the arguments,
            // as a second lower-case word.
            "DECIMAL UNSIGNED" => Some(format!("decimal({precision},{scale}) unsigned")),
            _ => None,
        };
    }
    if let Some(length) = column.character_length() {
        return match column.type_name() {
            "VARCHAR" => Some(format!("varchar({length})")),
            "CHAR" => Some(format!("char({length})")),
            _ => None,
        };
    }
    match column.type_name() {
        "TINYINT" => Some("tinyint".to_owned()),
        "SMALLINT" => Some("smallint".to_owned()),
        "MEDIUMINT" => Some("mediumint".to_owned()),
        "INT" | "INTEGER" => Some("int".to_owned()),
        "BIGINT" => Some("bigint".to_owned()),
        // Measured on MySQL 8.4.11: the sign prints as a second lower-case
        // word, and `INTEGER UNSIGNED` prints as `int unsigned` the way plain
        // `INTEGER` prints as `int`.
        "TINYINT UNSIGNED" => Some("tinyint unsigned".to_owned()),
        "SMALLINT UNSIGNED" => Some("smallint unsigned".to_owned()),
        "MEDIUMINT UNSIGNED" => Some("mediumint unsigned".to_owned()),
        "INT UNSIGNED" | "INTEGER UNSIGNED" => Some("int unsigned".to_owned()),
        "BIGINT UNSIGNED" => Some("bigint unsigned".to_owned()),
        "TEXT" => Some("text".to_owned()),
        "TINYTEXT" => Some("tinytext".to_owned()),
        "MEDIUMTEXT" => Some("mediumtext".to_owned()),
        "LONGTEXT" => Some("longtext".to_owned()),
        "BLOB" => Some("blob".to_owned()),
        "TINYBLOB" => Some("tinyblob".to_owned()),
        "MEDIUMBLOB" => Some("mediumblob".to_owned()),
        "LONGBLOB" => Some("longblob".to_owned()),
        "DOUBLE" => Some("double".to_owned()),
        "FLOAT" => Some("float".to_owned()),
        // Measured on MySQL 8.4.11: the sign prints as a second lower-case
        // word, as it does on an integer.
        "DOUBLE UNSIGNED" => Some("double unsigned".to_owned()),
        "FLOAT UNSIGNED" => Some("float unsigned".to_owned()),
        // Measured on MySQL 8.4.11: both BOOLEAN and BOOL print as this.
        "BOOLEAN" => Some("tinyint(1)".to_owned()),
        "DATETIME" => Some("datetime".to_owned()),
        "DATE" => Some("date".to_owned()),
        "TIME" => Some("time".to_owned()),
        "YEAR" => Some("year".to_owned()),
        // Measured on MySQL 8.4.11: a nullable TIMESTAMP prints its NULL, where
        // a nullable DATETIME prints only the DEFAULT.
        "TIMESTAMP" => Some("timestamp".to_owned()),
        "JSON" => Some("json".to_owned()),
        // An ENUM and a SET keep their members, and MySQL prints the keyword
        // in lower case with the members as they were written.
        other => member_type_name(other),
    }
}

fn member_type_name(declared: &str) -> Option<String> {
    let (keyword, members) = match turso_mysql_parser::set_members(declared) {
        Some(members) => ("set", members),
        None => ("enum", turso_mysql_parser::enum_members(declared)?),
    };
    Some({
        {
            format!(
                "{keyword}({})",
                members
                    .iter()
                    .map(|member| format!("'{member}'"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    })
}

fn quoted(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}
