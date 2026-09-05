// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Renders `SHOW CREATE TABLE` output.
//!
//! Every rule here was read off the pinned MySQL 8.4.11 golden bytes: two
//! spaces of indent, `,\n` between items, no trailing newline, lower-case type
//! names, and DEFAULT literals in single quotes even when they are numbers.

use crate::session::{MySqlColumnDefault, MySqlColumnKey, MySqlColumnMetadata};

/// The trailer MySQL puts after the closing parenthesis.
///
/// Turso is not InnoDB and does not use `utf8mb4_0900_ai_ci`, but MySQL always
/// sends these bytes and clients parse them, so the compatibility surface
/// repeats them verbatim.
const TABLE_TRAILER: &str = " ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

/// Renders the `Create Table` column of `SHOW CREATE TABLE`.
///
/// The table-level `AUTO_INCREMENT=<n>` MySQL adds once its counter passes one
/// is left out: Turso hands out auto-increment values in reserved ranges, so
/// the counter it stores is not the next value MySQL would print.
pub fn render_create_table(table: &str, columns: &[MySqlColumnMetadata]) -> Option<String> {
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
    Some(format!(
        "CREATE TABLE {} (\n{body}\n){TABLE_TRAILER}",
        quoted(table)
    ))
}

fn render_column(column: &MySqlColumnMetadata) -> Option<String> {
    let mut rendered = format!(
        "{} {}",
        quoted(column.name()),
        type_name(column.type_name())?
    );
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

fn type_name(declared: &str) -> Option<&'static str> {
    match declared {
        "TINYINT" => Some("tinyint"),
        "SMALLINT" => Some("smallint"),
        "MEDIUMINT" => Some("mediumint"),
        "INT" | "INTEGER" => Some("int"),
        "BIGINT" => Some("bigint"),
        "TEXT" => Some("text"),
        "BLOB" => Some("blob"),
        _ => None,
    }
}

fn quoted(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}
