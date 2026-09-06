// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Renders `SHOW CREATE TABLE` output.
//!
//! Every rule here was read off the pinned MySQL 8.4.11 golden bytes: two
//! spaces of indent, `,\n` between items, no trailing newline, lower-case type
//! names, and DEFAULT literals in single quotes even when they are numbers.

use crate::session::{MySqlColumnDefault, MySqlColumnKey, MySqlColumnMetadata};

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
    next_auto_increment: Option<u64>,
) -> Option<String> {
    if columns.is_empty() {
        return None;
    }
    let mut items = Vec::with_capacity(columns.len() + 1);
    for column in columns {
        items.push(render_column(column)?);
    }
    if let Some(primary) = columns
        .iter()
        .find(|column| column.key() == MySqlColumnKey::Primary)
    {
        items.push(format!("PRIMARY KEY ({})", quoted(primary.name())));
    }
    for unique in columns
        .iter()
        .filter(|column| column.key() == MySqlColumnKey::Unique)
    {
        // MySQL names a single-column index after the column it covers.
        items.push(format!(
            "UNIQUE KEY {} ({})",
            quoted(unique.name()),
            quoted(unique.name())
        ));
    }
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

fn render_column(column: &MySqlColumnMetadata) -> Option<String> {
    let mut rendered = format!("{} {}", quoted(column.name()), type_name(column)?);
    if !column.nullable() {
        rendered.push_str(" NOT NULL");
    }
    rendered.push_str(&render_default(column)?);
    match column.extra() {
        "" => {}
        "AUTO_INCREMENT" => rendered.push_str(" AUTO_INCREMENT"),
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
        if column.nullable() && !matches!(column.type_name(), "TEXT" | "BLOB") {
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
        "TEXT" => Some("text".to_owned()),
        "BLOB" => Some("blob".to_owned()),
        "DOUBLE" => Some("double".to_owned()),
        // Measured on MySQL 8.4.11: both BOOLEAN and BOOL print as this.
        "BOOLEAN" => Some("tinyint(1)".to_owned()),
        _ => None,
    }
}

fn quoted(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}
