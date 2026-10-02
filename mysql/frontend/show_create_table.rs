// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Renders `SHOW CREATE TABLE` output.
//!
//! Every rule here was read off the pinned MySQL 8.4.11 golden bytes: two
//! spaces of indent, `,\n` between items, no trailing newline, lower-case type
//! names, and DEFAULT literals in single quotes even when they are numbers.

use turso_mysql_parser::{MySqlTableCollation, MySqlTableOptions};

use crate::session::{MySqlColumnDefault, MySqlColumnMetadata, MySqlIndexEntry};

/// Renders the `Create Table` column of `SHOW CREATE TABLE`.
///
/// `next_auto_increment` is the value MySQL prints as `AUTO_INCREMENT=<n>`,
/// which it leaves out entirely while the counter is still at one.
pub fn render_create_table(
    table: &str,
    options: &MySqlTableOptions,
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
        items.push(render_column(column, options.collation)?);
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
    // Measured on MySQL 8.4.11: the collation is printed whatever it is, and
    // the comment after it, where the table has one.
    let comment = options
        .comment
        .as_deref()
        .map(|comment| {
            format!(
                " COMMENT={}",
                turso_mysql_parser::quoted_mysql_text(comment)
            )
        })
        .unwrap_or_default();
    Some(format!(
        "CREATE TABLE {} (\n{body}\n) ENGINE=InnoDB{counter} DEFAULT CHARSET={} COLLATE={}{}{comment}",
        quoted(table),
        options.collation.character_set(),
        options.collation.name(),
        options.written_row_format(),
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

impl MySqlForeignKey {
    pub fn of(key: &turso_core::schema::ForeignKey) -> Self {
        Self {
            name: key.name.clone(),
            declaration_order: key.decl_order,
            child_columns: key.child_columns.to_vec(),
            parent_table: key.parent_table.clone(),
            parent_columns: key.parent_columns.to_vec(),
            on_delete: mysql_reference_action(key.on_delete),
            on_update: mysql_reference_action(key.on_update),
        }
    }
}

/// Names a referential action the way MySQL prints it.
///
/// Measured on MySQL 8.4.11: `SHOW CREATE TABLE` prints nothing for the default
/// `NO ACTION`, written or not, and prints a `RESTRICT` that was written. The
/// rest are printed as written.
fn mysql_reference_action(action: turso_parser::ast::RefAct) -> Option<String> {
    match action {
        turso_parser::ast::RefAct::NoAction => None,
        turso_parser::ast::RefAct::Restrict => Some("RESTRICT".to_owned()),
        turso_parser::ast::RefAct::Cascade => Some("CASCADE".to_owned()),
        turso_parser::ast::RefAct::SetNull => Some("SET NULL".to_owned()),
        turso_parser::ast::RefAct::SetDefault => Some("SET DEFAULT".to_owned()),
    }
}

pub fn foreign_key_refusal_message(
    database: &str,
    refusal: &turso_core::ForeignKeyRefusal,
) -> String {
    let refused = match refusal.refused_row {
        turso_core::RefusedRow::ChildRowWithoutParent => "Cannot add or update a child row",
        turso_core::RefusedRow::ParentRowWithChildren => "Cannot delete or update a parent row",
    };
    format!(
        "{refused}: a foreign key constraint fails ({}.{}, {})",
        quoted(database),
        quoted(&refusal.child_table),
        render_foreign_key(
            &refusal.child_table,
            &MySqlForeignKey::of(&refusal.foreign_key)
        ),
    )
}

/// Renders one foreign key the way MySQL prints it.
///
/// Measured on MySQL 8.4.11: a constraint written without a name is printed as
/// `` CONSTRAINT `t_ibfk_1` FOREIGN KEY (`a`, `b`) REFERENCES `p` (`x`, `y`) ``,
/// numbered from one in declaration order, the columns parted by a comma and a
/// space, and one written with a name is printed under the name it was given.
fn render_foreign_key(table: &str, key: &MySqlForeignKey) -> String {
    let columns = |names: &[String]| {
        names
            .iter()
            .map(|name| quoted(name))
            .collect::<Vec<_>>()
            .join(", ")
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

fn render_column(
    column: &MySqlColumnMetadata,
    table_collation: MySqlTableCollation,
) -> Option<String> {
    let mut rendered = format!("{} {}", quoted(column.name()), type_name(column)?);
    rendered.push_str(&collation_clause(column.collation_name(), table_collation));
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
        extra
            if extra
                .strip_prefix("DEFAULT_GENERATED ")
                .unwrap_or(extra)
                .strip_prefix("on update ")
                == Some(the_moment(column.temporal_precision()).as_str()) =>
        {
            rendered.push_str(" ON UPDATE ");
            rendered.push_str(&the_moment(column.temporal_precision()));
        }
        _ => return None,
    }
    // Measured: a comment is printed last, after every other attribute, and an
    // empty one is not printed at all.
    if !column.comment().is_empty() {
        rendered.push_str(&format!(
            " COMMENT {}",
            turso_mysql_parser::quoted_mysql_text(column.comment())
        ));
    }
    Some(rendered)
}

/// What a column of words says about its collation.
///
/// Measured on MySQL 8.4.11: a column whose collation differs from its table's
/// prints ` CHARACTER SET <its character set> COLLATE <name>` — `utf8mb3` for
/// a `utf8mb3_unicode_ci` column in a `utf8mb4` table; one that takes a table
/// collation other than `utf8mb4_0900_ai_ci` prints ` COLLATE <name>`; and one
/// that takes `utf8mb4_0900_ai_ci` from its table prints nothing. MySQL also
/// prints the longer form for a column that named its table's collation
/// itself, which this does not remember: such a column prints the shorter one.
fn collation_clause(column: Option<&str>, table: MySqlTableCollation) -> String {
    match column {
        None => String::new(),
        Some(column) if column != table.name() => {
            format!(
                " CHARACTER SET {} COLLATE {column}",
                turso_mysql_parser::character_set_of_collation(column)
            )
        }
        Some(_) if table == MySqlTableCollation::default() => String::new(),
        Some(column) => format!(" COLLATE {column}"),
    }
}

/// The words naming the moment a statement runs at, as a column holding
/// `digits` fractional-second digits reads it. Measured on MySQL 8.4.11: a
/// `datetime(3)` reads `CURRENT_TIMESTAMP(3)` in `SHOW CREATE TABLE`, `SHOW
/// COLUMNS` and `information_schema.COLUMNS` alike.
pub fn the_moment(digits: Option<u8>) -> String {
    match digits {
        None | Some(0) => "CURRENT_TIMESTAMP".to_owned(),
        Some(digits) => format!("CURRENT_TIMESTAMP({digits})"),
    }
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
        // Measured on MySQL 8.4.11: a bit prints as a bit literal, unquoted.
        MySqlColumnDefault::Integer { .. } if column.type_name() == "BIT" => {
            format!(" DEFAULT {}", bit_literal(default)?)
        }
        // A written number belongs to a column that holds one, and MySQL
        // prints it at that column's own scale.
        MySqlColumnDefault::Integer { text, .. } | MySqlColumnDefault::Number(text) => {
            format!(
                " DEFAULT '{}'",
                at_the_columns_scale(column.decimal_size(), text)
            )
        }
        MySqlColumnDefault::Boolean(value) => {
            format!(" DEFAULT '{}'", u8::from(*value))
        }
        // MySQL prints this one without quotes, it naming a moment rather than
        // holding a value.
        MySqlColumnDefault::Moment => {
            format!(" DEFAULT {}", the_moment(column.temporal_precision()))
        }
        // Measured on MySQL 8.4.11: an expression default prints in its
        // parentheses, the call lowercased.
        MySqlColumnDefault::MomentCall => " DEFAULT (now())".to_owned(),
        // Measured on MySQL 8.4.11, and it is the rule a column's comment is
        // written by: a quote is doubled, a backslash written twice, and a
        // newline, a carriage return and a zero byte each named. A `DEFAULT`
        // written this way is what every `ENUM` column carrying one prints,
        // and what a word column's own default prints.
        MySqlColumnDefault::Text(text) => {
            if column.decimal_size().is_some() {
                format!(
                    " DEFAULT '{}'",
                    at_the_columns_scale(column.decimal_size(), text)
                )
            } else {
                format!(" DEFAULT {}", turso_mysql_parser::quoted_mysql_text(text))
            }
        }
    })
}

/// The default of a `BIT(1)` column as MySQL writes it, `b'0'` or `b'1'`, the
/// same in `SHOW CREATE TABLE`, `SHOW COLUMNS` and
/// `information_schema.COLUMNS`; `None` for a default no bit holds.
pub fn bit_literal(default: &MySqlColumnDefault) -> Option<&'static str> {
    match default {
        MySqlColumnDefault::Integer { value: 0, .. } => Some("b'0'"),
        MySqlColumnDefault::Integer { value: 1, .. } => Some("b'1'"),
        _ => None,
    }
}

/// One written number at the scale its column holds values at.
///
/// Measured on MySQL 8.4.11: a `DECIMAL` column keeps its default at its own
/// scale and prints it back that way — `DECIMAL(10,2) DEFAULT 3` prints
/// `'3.00'` and `DEFAULT 1.5` on a `DECIMAL(6,3)` prints `'1.500'`. A column
/// with no scale of its own, a `DOUBLE` among them, prints what was written.
pub fn at_the_columns_scale(scale: Option<(u32, u32)>, written: &str) -> String {
    let Some((precision, scale)) = scale else {
        return written.to_owned();
    };
    turso_mysql_parser::round_decimal_to_scale(written, precision, scale)
        .unwrap_or_else(|_| written.to_owned())
}

/// Renders the type the way MySQL 8.4.11 prints it here, lower case and
/// carrying the declared length where the type has one.
///
/// Every reading of a column goes through this one — `SHOW CREATE TABLE`,
/// `SHOW COLUMNS` and `DESCRIBE` all print the same text for a type, measured,
/// and a second table of the same names drifted behind this one by five types
/// until it was taken away.
pub fn type_name(column: &MySqlColumnMetadata) -> Option<String> {
    if let Some(precision) = column.temporal_precision() {
        let name = column.type_name().to_ascii_lowercase();
        return Some(if precision == 0 {
            name
        } else {
            format!("{name}({precision})")
        });
    }
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
            "VARBINARY" => Some(format!("varbinary({length})")),
            "BINARY" => Some(format!("binary({length})")),
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
        "BIT" => Some("bit(1)".to_owned()),
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
