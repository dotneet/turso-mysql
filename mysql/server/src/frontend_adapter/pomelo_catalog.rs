//! Answering the `information_schema` reads Entity Framework Core's `dbcontext
//! scaffold` sends through Pomelo 9 and MySqlConnector, all as text.
//!
//! Each answer is worked out from plain reads of the catalog, which the text
//! path authorizes as it authorizes any read. Every row, order and column
//! description was measured on a MySQL 8.4.11 initialized with
//! `lower_case_table_names=1`, the rule this server reports: under it the
//! table names come back lower-cased, as they do here, and several columns are
//! described differently from a server under 0.

use super::prisma_catalog::{fields, whole_number, written_name};
use super::*;
use turso_mysql_parser::PomeloInformationSchemaQuery;

/// One row as the catalog's text path read it, a value's text or NULL.
type ReadRow = Vec<Option<Vec<u8>>>;

/// Rows gathered the way a `GROUP BY` gathers them: what the group is keyed
/// on, and for each row its column's place in the key and the words it lists.
type Groups<const N: usize> = Vec<(ReadRow, Vec<(i64, [Vec<u8>; N])>)>;

/// The `group_concat_max_len` these reads are answered under, MySQL's own:
/// the widths MySQL reports for the joined lists were measured under it.
const GROUP_CONCAT_MAX_LEN: usize = 1024;

impl<A> AuthorizedDatabaseCommandAdapter<A>
where
    A: DatabaseAuthorizer,
{
    /// Answers one of the scaffold's catalog reads.
    pub(super) fn pomelo_catalog_result(
        &mut self,
        query: &PomeloInformationSchemaQuery,
    ) -> Result<TextResultSet, FrontendErrorKind> {
        if !matches!(query, PomeloInformationSchemaQuery::Tables)
            && self.session_variables.group_concat_max_len() != GROUP_CONCAT_MAX_LEN as u64
        {
            return Err(FrontendErrorKind::Unsupported);
        }
        let rows = match query {
            PomeloInformationSchemaQuery::Tables => self.pomelo_tables()?,
            PomeloInformationSchemaQuery::PrimaryKey { schema, table } => {
                self.pomelo_indexes(schema, table, true)?
            }
            PomeloInformationSchemaQuery::Indexes { schema, table } => {
                self.pomelo_indexes(schema, table, false)?
            }
            PomeloInformationSchemaQuery::ForeignKeys { schema, table } => {
                self.pomelo_foreign_keys(schema, table)?
            }
        };
        Ok(TextResultSet {
            columns: pomelo_catalog_columns(query),
            rows,
            warnings: 0,
            status_flags: self.status_flags(),
        })
    }

    /// The base tables and views of the selected database in the order of
    /// their names, a view's comment of `VIEW` answered as the empty word and
    /// each collation beside its character set, which is the one row
    /// `COLLATION_CHARACTER_SET_APPLICABILITY` holds for it. A view has no
    /// collation, so the `LEFT JOIN` finds no character set for it.
    fn pomelo_tables(&mut self) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let mut tables = Vec::new();
        for row in self.read_the_catalog(
            "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT, TABLE_COLLATION \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = SCHEMA()",
        )? {
            let [name, kind, comment, collation] = fields(row)?;
            let kind = kind.ok_or(FrontendErrorKind::Internal)?;
            if kind != b"BASE TABLE" && kind != b"VIEW" {
                continue;
            }
            let comment = match comment {
                Some(comment) if comment == b"VIEW" && kind == b"VIEW" => Some(Vec::new()),
                comment => comment,
            };
            let character_set = match &collation {
                Some(collation) => Some(pomelo_character_set_of(collation)?.as_bytes().to_vec()),
                None => None,
            };
            tables.push(vec![name, Some(kind), comment, character_set, collation]);
        }
        tables.sort_by(|left, right| left[0].cmp(&right[0]));
        Ok(tables)
    }

    /// The table's primary key, or every other index, one row each: the
    /// columns, their prefix lengths (0 for a whole column) and, beside the
    /// primary key, their sort orders, each joined by commas in the order of
    /// the columns' places in the index.
    ///
    /// Measured on MySQL 8.4.11: the indexes come in the order of their names
    /// read without regard to case.
    fn pomelo_indexes(
        &mut self,
        schema: &str,
        table: &str,
        primary: bool,
    ) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let read = self.read_the_catalog(&format!(
            "SELECT INDEX_NAME, NON_UNIQUE, INDEX_TYPE, SEQ_IN_INDEX, COLUMN_NAME, SUB_PART, \
             COLLATION FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
            written_name(schema)?,
            written_name(table)?
        ))?;
        let mut groups: Groups<3> = Vec::new();
        for row in read {
            let [name, non_unique, kind, place, column, prefix, order] = fields(row)?;
            let name = name.ok_or(FrontendErrorKind::Internal)?;
            if name.eq_ignore_ascii_case(b"PRIMARY") != primary {
                continue;
            }
            let key = if primary {
                vec![Some(name)]
            } else {
                vec![Some(name), non_unique, kind]
            };
            let member = (
                whole_number(place.as_deref())?,
                [
                    column.ok_or(FrontendErrorKind::Internal)?,
                    prefix.unwrap_or_else(|| b"0".to_vec()),
                    order.unwrap_or_else(|| b"A".to_vec()),
                ],
            );
            match groups.iter_mut().find(|(grouped, _)| *grouped == key) {
                Some((_, members)) => members.push(member),
                None => groups.push((key, vec![member])),
            }
        }
        sort_by_name_without_case(&mut groups)?;
        groups
            .into_iter()
            .map(|(key, mut members)| {
                members.sort_by_key(|(place, _)| *place);
                let [columns, prefixes, orders] =
                    [0, 1, 2].map(|list| joined(members.iter().map(|(_, values)| &values[list])));
                let mut key = key.into_iter();
                let name = key.next().ok_or(FrontendErrorKind::Internal)?;
                Ok(if primary {
                    vec![name, Some(columns?), Some(prefixes?)]
                } else {
                    let (non_unique, kind) = (key.next().flatten(), key.next().flatten());
                    vec![
                        name,
                        non_unique,
                        Some(columns?),
                        Some(prefixes?),
                        Some(orders?),
                        kind,
                    ]
                })
            })
            .collect()
    }

    /// The table's foreign keys, one row each, with each column and the
    /// parent column it names joined by `|`, the pairs joined by commas in
    /// the order of their places in the key, and the key's `ON DELETE` rule.
    ///
    /// Measured on MySQL 8.4.11: the keys come in the order of their names
    /// read without regard to case.
    fn pomelo_foreign_keys(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let written_schema = written_name(schema)?;
        let read = self.read_the_catalog(&format!(
            "SELECT CONSTRAINT_SCHEMA, CONSTRAINT_NAME, TABLE_NAME, REFERENCED_TABLE_NAME, \
             ORDINAL_POSITION, COLUMN_NAME, REFERENCED_COLUMN_NAME \
             FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = {written_schema} AND TABLE_NAME = {}",
            written_name(table)?
        ))?;
        let rules = self.read_the_catalog(&format!(
            "SELECT CONSTRAINT_SCHEMA, CONSTRAINT_NAME, DELETE_RULE \
             FROM information_schema.REFERENTIAL_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = {written_schema}"
        ))?;
        let mut groups: Groups<1> = Vec::new();
        for row in read {
            let [key_schema, name, key_table, parent, place, column, parent_column] = fields(row)?;
            let name = name.ok_or(FrontendErrorKind::Internal)?;
            if name.eq_ignore_ascii_case(b"PRIMARY") || parent.is_none() {
                continue;
            }
            let mut pair = column.ok_or(FrontendErrorKind::Internal)?;
            if let Some(parent_column) = parent_column {
                pair.push(b'|');
                pair.extend(parent_column);
            }
            // The key's schema is grouped on but not answered, so it goes last
            // and the name the groups are ordered by stays first.
            let key = vec![Some(name), key_table, parent, key_schema];
            let member = (whole_number(place.as_deref())?, [pair]);
            match groups.iter_mut().find(|(grouped, _)| *grouped == key) {
                Some((_, members)) => members.push(member),
                None => groups.push((key, vec![member])),
            }
        }
        sort_by_name_without_case(&mut groups)?;
        groups
            .into_iter()
            .map(|(key, mut members)| {
                members.sort_by_key(|(place, _)| *place);
                let [name, key_table, parent, key_schema] =
                    <[Option<Vec<u8>>; 4]>::try_from(key)
                        .map_err(|_| FrontendErrorKind::Internal)?;
                let pairs = joined(members.iter().map(|(_, [pair])| pair))?;
                let rule = delete_rule(&rules, key_schema.as_deref(), name.as_deref())?;
                Ok(vec![name, key_table, parent, Some(pairs), rule])
            })
            .collect()
    }
}

/// The character set a table's collation belongs to. Every collation a table
/// takes here is one of MySQL's, whose names start with their character set's.
fn pomelo_character_set_of(collation: &[u8]) -> Result<&'static str, FrontendErrorKind> {
    let collation = std::str::from_utf8(collation).map_err(|_| FrontendErrorKind::Internal)?;
    let character_set = turso_mysql_parser::character_set_of_collation(collation);
    if !collation.starts_with(&format!("{character_set}_")) {
        return Err(FrontendErrorKind::Unsupported);
    }
    Ok(character_set)
}

/// Orders groups whose first key value is a name as MySQL orders them, by the
/// name read without regard to case. MySQL folds case by its own table beyond
/// ASCII, so a name outside it is refused where the order would turn on it.
fn sort_by_name_without_case<T>(groups: &mut [(ReadRow, T)]) -> Result<(), FrontendErrorKind> {
    let name = |key: &ReadRow| key.first().cloned().flatten().unwrap_or_default();
    if groups.len() > 1 && groups.iter().any(|(key, _)| !name(key).is_ascii()) {
        return Err(FrontendErrorKind::Unsupported);
    }
    groups.sort_by_key(|(key, _)| name(key).to_ascii_lowercase());
    Ok(())
}

/// A list `GROUP_CONCAT` joins with commas. One longer than
/// `group_concat_max_len` is refused: MySQL cuts it and warns.
fn joined<'a>(values: impl Iterator<Item = &'a Vec<u8>>) -> Result<Vec<u8>, FrontendErrorKind> {
    let joined = values.cloned().collect::<Vec<_>>().join(&b","[..]);
    if joined.len() > GROUP_CONCAT_MAX_LEN {
        return Err(FrontendErrorKind::Unsupported);
    }
    Ok(joined)
}

/// The `ON DELETE` rule the scaffold's correlated subquery finds for a key:
/// NULL where `REFERENTIAL_CONSTRAINTS` has no row for it. Two rows would be
/// MySQL's 1242, which a constraint name unique in its database never meets.
fn delete_rule(
    rules: &[ReadRow],
    schema: Option<&[u8]>,
    name: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, FrontendErrorKind> {
    let same = |left: Option<&[u8]>, right: Option<&[u8]>| match (left, right) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => false,
    };
    let mut found = rules
        .iter()
        .filter(|rule| same(rule[0].as_deref(), schema) && same(rule[1].as_deref(), name));
    let rule = found.next().map(|rule| rule[2].clone());
    if found.next().is_some() {
        return Err(FrontendErrorKind::Unsupported);
    }
    Ok(rule.flatten())
}

/// The columns each read answers, as MySQL 8.4.11 under
/// `lower_case_table_names=1` described them over the text protocol with
/// `group_concat_max_len` at 1024.
fn pomelo_catalog_columns(query: &PomeloInformationSchemaQuery) -> Vec<ColumnDefinitionConfig> {
    let words = |name: &str, length: u32| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = length;
        column
    };
    let read = |name: &str, table: &str, original_table: &str, schema: &str, length: u32| {
        let mut column = words(name, length);
        column.original_name = name.into();
        column.table = table.into();
        column.original_table = original_table.into();
        column.schema = schema.into();
        column
    };
    let worked_out = |name: &str, length: u32| {
        let mut column = words(name, length);
        column.decimals = NOT_FIXED_DECIMALS;
        column
    };
    let joined = |name: &str, length: u32| {
        let mut column = worked_out(name, length);
        column.column_type = MYSQL_TYPE_LONG_BLOB;
        column
    };
    let catalog = "information_schema";
    match query {
        PomeloInformationSchemaQuery::Tables => {
            let mut name = read("TABLE_NAME", "t", "TABLES", "", 256);
            name.decimals = NOT_FIXED_DECIMALS;
            let mut kind = read("TABLE_TYPE", "t", "TABLES", catalog, 44);
            kind.column_type = MYSQL_TYPE_STRING;
            kind.flags = MYSQL_NOT_NULL_FLAG
                | MYSQL_MULTIPLE_KEY_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_ENUM_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG;
            let key = MYSQL_UNIQUE_KEY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG;
            let mut character_set = read(
                "TABLE_CHARACTER_SET",
                "ccsa",
                "COLLATION_CHARACTER_SET_APPLICABILITY",
                catalog,
                256,
            );
            character_set.original_name = "CHARACTER_SET_NAME".into();
            character_set.flags = key;
            let mut collation = read("TABLE_COLLATION", "t", "TABLES", catalog, 256);
            collation.flags = key;
            vec![
                name,
                kind,
                worked_out("TABLE_COMMENT", 8192),
                character_set,
                collation,
            ]
        }
        PomeloInformationSchemaQuery::PrimaryKey { .. } => {
            let mut name = read("INDEX_NAME", "STATISTICS", "STATISTICS", "", 256);
            name.decimals = NOT_FIXED_DECIMALS;
            vec![name, joined("COLUMNS", 36864), joined("SUB_PARTS", 65536)]
        }
        PomeloInformationSchemaQuery::Indexes { .. } => {
            let mut non_unique = read("NON_UNIQUE", "STATISTICS", "", catalog, 2);
            non_unique.column_type = MYSQL_TYPE_LONG;
            non_unique.character_set = MYSQL_BINARY_COLLATION;
            non_unique.flags = MYSQL_NOT_NULL_FLAG;
            let mut kind = read("INDEX_TYPE", "STATISTICS", "", catalog, 44);
            kind.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG;
            vec![
                read("INDEX_NAME", "STATISTICS", "", catalog, 256),
                non_unique,
                joined("COLUMNS", 36864),
                joined("SUB_PARTS", 65536),
                joined("COLLATION", 65536),
                kind,
            ]
        }
        PomeloInformationSchemaQuery::ForeignKeys { .. } => {
            let key = |name: &str| read(name, "KEY_COLUMN_USAGE", "", catalog, 256);
            let mut rule = words("DELETE_RULE", 44);
            rule.original_name = "DELETE_RULE".into();
            rule.flags = MYSQL_BINARY_FLAG;
            vec![
                key("CONSTRAINT_NAME"),
                key("TABLE_NAME"),
                key("REFERENCED_TABLE_NAME"),
                joined("PAIRED_COLUMNS", 36864),
                rule,
            ]
        }
    }
}
