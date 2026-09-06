//! Reading the schema for the questions clients ask about it.
//!
//! `SHOW TABLES`, `SHOW COLUMNS`, `SHOW INDEX` and `SHOW CREATE TABLE` all
//! answer from the persisted SQLite schema rather than from a user table, and
//! they share the awkward parts: a view has no stored columns, so its shape has
//! to be worked out from its projection, and every scan is capped so a huge
//! schema cannot produce an unbounded listing.

use super::*;

impl MySqlConnection {
    /// Lists user-visible tables and views from the current database catalog.
    ///
    /// This reads the persisted schema directly through the trusted Core
    /// connection. SQLite and Turso internal tables are deliberately omitted.
    pub fn list_tables(&self) -> Result<Vec<MySqlTable>> {
        let sql = format!(
            "SELECT name, type FROM sqlite_schema \
             WHERE type IN ('table', 'view') \
             AND lower(name) NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             AND lower(name) NOT LIKE '\\_\\_turso\\_internal\\_%' ESCAPE '\\' \
             LIMIT {TABLE_LIST_SCAN_LIMIT}"
        );
        let rows = self.inner.prepare(&sql)?.run_collect_rows()?;
        if Self::table_list_is_truncated(rows.len()) {
            return Err(LimboError::TooBig);
        }
        let mut tables = Vec::with_capacity(rows.len());
        for row in rows {
            let [name, kind] = row.as_slice() else {
                return Err(LimboError::Corrupt(
                    "sqlite_schema table listing row has an invalid shape".to_string(),
                ));
            };
            let name = name.to_text().ok_or_else(|| {
                LimboError::Corrupt("sqlite_schema table name is not text".to_string())
            })?;
            assert!(
                !turso_core::schema::is_system_table(name),
                "fixed table-list query must exclude internal tables"
            );
            let kind = match kind.to_text() {
                Some("table") => MySqlTableKind::BaseTable,
                Some("view") => MySqlTableKind::View,
                _ => {
                    return Err(LimboError::Corrupt(
                        "sqlite_schema table listing kind is invalid".to_string(),
                    ));
                }
            };
            tables.push(MySqlTable {
                name: name.to_owned(),
                kind,
            });
        }
        tables.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(tables)
    }

    /// Renders `SHOW CREATE TABLE` for one base table in the selected database.
    pub fn show_create_table(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<MySqlShowCreateTableResult, MySqlShowCreateTableError> {
        match self.stored_object_kind(table)? {
            None => return Err(MySqlShowCreateTableError::MissingTable),
            Some(MySqlTableKind::View) => return Err(MySqlShowCreateTableError::NotTable),
            Some(MySqlTableKind::BaseTable) => {}
        }
        // list_columns accepts CHECK and FOREIGN KEY but drops them, and there
        // is nowhere to put them in the rendered DDL yet. Printing the table
        // without them would claim constraints that exist do not.
        let schema = self.inner.current_schema();
        let unprintable_constraints = schema
            .get_table(table.as_str())
            .and_then(|core_table| core_table.btree())
            .is_some_and(|btree| {
                !btree.check_constraints.is_empty() || !btree.foreign_keys.is_empty()
            });
        if unprintable_constraints {
            return Err(MySqlShowCreateTableError::Unsupported);
        }
        let columns = self.list_columns(table).map_err(|error| match error {
            MySqlColumnMetadataError::Engine(error) => MySqlShowCreateTableError::Engine(error),
            MySqlColumnMetadataError::TableNotFound => MySqlShowCreateTableError::MissingTable,
            MySqlColumnMetadataError::CorruptDefinition
            | MySqlColumnMetadataError::UnsupportedDefinition => {
                MySqlShowCreateTableError::Unsupported
            }
        })?;
        let indexes = self.list_indexes(table)?;
        let next_auto_increment = self.next_auto_increment_value(table)?;
        let create_statement = crate::show_create_table::render_create_table(
            table.as_str(),
            &columns,
            &indexes,
            next_auto_increment,
        )
        .ok_or(MySqlShowCreateTableError::Unsupported)?;
        Ok(MySqlShowCreateTableResult {
            table: table.as_str().to_owned(),
            create_statement,
        })
    }

    /// The number MySQL would print as `AUTO_INCREMENT=<n>`: one past the
    /// highest value handed out so far.
    ///
    /// `None` when the table has no auto-increment column, when nothing has
    /// been handed out yet — which is where MySQL leaves the trailer off — or
    /// when a concurrent INSERT holds the allocator. That last case prints no
    /// counter rather than failing a catalog read MySQL always answers; the
    /// number is a snapshot either way.
    fn next_auto_increment_value(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<Option<u64>, MySqlShowCreateTableError> {
        let auto_increment = self
            .load_auto_increment_table(table.as_str())
            .map_err(MySqlShowCreateTableError::Engine)?;
        let Some(auto_increment) = auto_increment else {
            return Ok(None);
        };
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            MySqlShowCreateTableError::Engine(LimboError::Corrupt(
                "AUTO_INCREMENT table without a registry-backed allocator capability".to_string(),
            ))
        })?;
        for _ in 0..ALLOCATOR_PEEK_ATTEMPTS {
            let mut query = capability
                .allocator
                .peek_high_water(auto_increment.key)
                .map_err(MySqlShowCreateTableError::Engine)?;
            match capability.io.block(|| query.step()) {
                Ok(high_water) => return Ok((high_water > 0).then_some(high_water + 1)),
                Err(LimboError::Busy) => std::thread::yield_now(),
                Err(error) => return Err(MySqlShowCreateTableError::Engine(error)),
            }
        }
        Ok(None)
    }

    /// Lists the indexes of one base table, the way `SHOW INDEX` reports them.
    ///
    /// MySQL puts the primary key first and the other unique indexes before the
    /// non-unique ones. A rowid-alias primary key has no index of its own in
    /// the engine, so it is reported from the table's primary key columns.
    pub fn list_indexes(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<Vec<MySqlIndexEntry>, MySqlShowCreateTableError> {
        match self.stored_object_kind(table)? {
            None => return Err(MySqlShowCreateTableError::MissingTable),
            Some(MySqlTableKind::View) => return Err(MySqlShowCreateTableError::NotTable),
            Some(MySqlTableKind::BaseTable) => {}
        }
        let schema = self.inner.current_schema();
        let core_table = schema
            .get_table(table.as_str())
            .ok_or(MySqlShowCreateTableError::MissingTable)?;
        let nullable = |column_name: &str| {
            core_table
                .columns()
                .iter()
                .find(|column| {
                    column
                        .name
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(column_name))
                })
                .is_none_or(|column| !column.notnull())
        };

        let mut primary = Vec::new();
        if let Some(btree) = core_table.btree() {
            for (position, (column_name, _)) in btree.primary_key_columns.iter().enumerate() {
                primary.push(MySqlIndexEntry {
                    key_name: "PRIMARY".to_owned(),
                    column_name: column_name.clone(),
                    sequence_in_index: position as u32 + 1,
                    unique: true,
                    nullable: nullable(column_name),
                });
            }
        }

        // MySQL lists the unique indexes before the non-unique ones and keeps
        // each group in creation order, which is the order the engine allocated
        // their root pages in.
        let mut indexes = schema.get_indices(table.as_str()).collect::<Vec<_>>();
        indexes.sort_by_key(|index| index.root_page);

        let mut unique = Vec::new();
        let mut secondary = Vec::new();
        for index in indexes {
            // The engine's own index behind a primary key is already reported.
            if index
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .eq(primary.iter().map(|entry| entry.column_name.as_str()))
            {
                continue;
            }
            let key_name = mysql_index_name(index);
            let rows = index
                .columns
                .iter()
                .enumerate()
                .map(|(position, column)| MySqlIndexEntry {
                    key_name: key_name.clone(),
                    column_name: column.name.clone(),
                    sequence_in_index: position as u32 + 1,
                    unique: index.unique,
                    nullable: nullable(&column.name),
                });
            if index.unique {
                unique.extend(rows);
            } else {
                secondary.extend(rows);
            }
        }
        primary.extend(unique);
        primary.extend(secondary);
        Ok(primary)
    }

    /// Reads whether one name is a stored table or view, hiding internal objects.
    fn stored_object_kind(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<Option<MySqlTableKind>, MySqlShowCreateTableError> {
        let table_name = table.as_str();
        if turso_core::schema::is_system_table(table_name) {
            return Ok(None);
        }
        let sql = format!(
            "SELECT type FROM sqlite_schema \
             WHERE type IN ('table', 'view') AND lower(name) = '{table_name}' LIMIT 2"
        );
        let rows = self
            .inner
            .prepare_internal(&sql)
            .map_err(MySqlShowCreateTableError::Engine)?
            .run_collect_rows()
            .map_err(MySqlShowCreateTableError::Engine)?;
        match rows.as_slice() {
            [] => Ok(None),
            [row] => match row.as_slice() {
                [Value::Text(kind)] => match kind.as_str() {
                    "table" => Ok(Some(MySqlTableKind::BaseTable)),
                    "view" => Ok(Some(MySqlTableKind::View)),
                    _ => Err(MySqlShowCreateTableError::Unsupported),
                },
                _ => Err(MySqlShowCreateTableError::Unsupported),
            },
            _ => Err(MySqlShowCreateTableError::Unsupported),
        }
    }

    /// Reconstructs the initial MySQL column metadata surface for one table or view.
    ///
    /// The normalized MySQL DDL is the source for MySQL-only fields. Core is
    /// used only to verify that the marked catalog row still describes the
    /// loaded table and its columns in the same order.
    pub fn list_columns(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<Vec<MySqlColumnMetadata>, MySqlColumnMetadataError> {
        let table_name = table.as_str();
        if turso_core::schema::is_system_table(table_name) {
            return Err(MySqlColumnMetadataError::TableNotFound);
        }
        let sql = format!(
            "SELECT name, type, sql, rootpage FROM sqlite_schema \
             WHERE type IN ('table', 'view') AND lower(name) = '{table_name}' LIMIT 2"
        );
        let rows = self
            .inner
            .prepare_internal(&sql)
            .map_err(MySqlColumnMetadataError::Engine)?
            .run_collect_rows()
            .map_err(MySqlColumnMetadataError::Engine)?;
        let row = match rows.as_slice() {
            [] => return Err(MySqlColumnMetadataError::TableNotFound),
            [row] => row,
            _ => return Err(MySqlColumnMetadataError::CorruptDefinition),
        };
        let [catalog_name, object_type, stored_sql, root_page] = row.as_slice() else {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        };
        let catalog_name = catalog_name
            .to_text()
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        let object_type = object_type
            .to_text()
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        let stored_sql = stored_sql
            .to_text()
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if object_type.eq_ignore_ascii_case("view") {
            // Core stores rootpage as integer zero for views, not SQL NULL.
            if root_page.as_int() != Some(0) {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
            return self.list_view_columns(table_name, catalog_name, stored_sql);
        }
        let root_page = root_page
            .as_int()
            .filter(|root_page| *root_page > 0)
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if !catalog_name.eq_ignore_ascii_case(table_name)
            || !object_type.eq_ignore_ascii_case("table")
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }

        let decoded = decode_schema_sql(SchemaSqlKind::Table, stored_sql)
            .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?
            .ok_or(MySqlColumnMetadataError::UnsupportedDefinition)?;
        let mode = SessionSqlMode {
            ansi_quotes: decoded.context.sql_mode.ansi_quotes,
            no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
        };
        let (statement, auto_increment_column_ordinal) = match decoded.v2_metadata() {
            Some(metadata) => {
                let Some(validation_context) = self.inner.schema_catalog_validation_context()
                else {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                };
                if metadata.database_id.into_bytes() != *validation_context.database_identity() {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                }
                let checked = parse_auto_increment_create_table(decoded.normalized_ddl, mode)
                    .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?;
                if checked.normalized_mysql_ddl != decoded.normalized_ddl {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                }

                // The checked parser proves the marker belongs to the
                // allocator column. Remove it from the canonical copy so the
                // general metadata parser can retain the original INT/INTEGER
                // spelling while the checked ordinal supplies the key/extra.
                const AUTO_INCREMENT_PRIMARY_KEY: &str = " AUTO_INCREMENT PRIMARY KEY";
                let mut normalized_without_auto_increment = checked.normalized_mysql_ddl.clone();
                let Some(marker_start) = find_unquoted_sql_fragment(
                    &normalized_without_auto_increment,
                    AUTO_INCREMENT_PRIMARY_KEY,
                    mode.no_backslash_escapes,
                ) else {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                };
                if find_unquoted_sql_fragment(
                    &normalized_without_auto_increment
                        [marker_start + AUTO_INCREMENT_PRIMARY_KEY.len()..],
                    AUTO_INCREMENT_PRIMARY_KEY,
                    mode.no_backslash_escapes,
                )
                .is_some()
                {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                }
                normalized_without_auto_increment.replace_range(
                    marker_start..marker_start + AUTO_INCREMENT_PRIMARY_KEY.len(),
                    "",
                );
                let statement = parse_create_table_ast(&normalized_without_auto_increment, mode)
                    .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?;
                (statement, Some(checked.allocator_column_ordinal))
            }
            None => (
                parse_create_table_ast(decoded.normalized_ddl, mode)
                    .map_err(mysql_metadata_parse_error)?,
                None,
            ),
        };
        let Stmt::CreateTable {
            temporary,
            tbl_name,
            body,
            ..
        } = statement
        else {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        };
        if temporary
            || tbl_name.db_name.is_some()
            || tbl_name.alias.is_some()
            || !tbl_name.name.as_str().eq_ignore_ascii_case(catalog_name)
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let CreateTableBody::ColumnsAndConstraints {
            columns,
            constraints,
            options,
        } = body
        else {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if columns.is_empty()
            || options.without_rowid_text.is_some()
            || options.strict_text.is_some()
            || constraints.iter().any(|constraint| {
                !matches!(
                    constraint.constraint,
                    turso_parser::ast::TableConstraint::Check { .. }
                        | turso_parser::ast::TableConstraint::ForeignKey { .. }
                )
            })
        {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }

        let schema = self.inner.current_schema();
        let core_table = schema
            .get_table(catalog_name)
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if core_table
            .get_root_page()
            .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?
            != root_page
            || schema.table_sql(catalog_name) != Some(stored_sql)
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let core_columns = core_table.columns();
        if core_columns.len() != columns.len() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        if columns
            .iter()
            .zip(core_columns)
            .any(|(column, core_column)| {
                core_column.name.as_deref() != Some(column.col_name.as_str())
            })
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }

        let mut metadata = columns
            .iter()
            .map(mysql_column_metadata)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if let Some(ordinal) = auto_increment_column_ordinal {
            let column = metadata
                .get_mut(ordinal)
                .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
            if column.key != MySqlColumnKey::None || column.nullable {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
            column.key = MySqlColumnKey::Primary;
            "AUTO_INCREMENT".clone_into(&mut column.extra);
        }
        let rowid_alias_ordinal = core_table
            .btree()
            .and_then(|table| table.get_rowid_alias_column().map(|(ordinal, _)| ordinal));
        if auto_increment_column_ordinal.is_none() && rowid_alias_ordinal.is_some() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        self.verify_column_indexes(table_name, &metadata, rowid_alias_ordinal)?;
        if core_columns
            .iter()
            .zip(&metadata)
            .enumerate()
            .any(|(ordinal, (core_column, column))| {
                (Some(ordinal) != auto_increment_column_ordinal
                    && core_column.notnull() == column.nullable)
                    || core_column.unique() != (column.key == MySqlColumnKey::Unique)
                    || core_column.primary_key() != (column.key == MySqlColumnKey::Primary)
            })
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        self.mark_indexed_columns(table, &mut metadata)?;
        Ok(metadata)
    }

    /// Reports a column that leads an index as `MUL`, the way MySQL does.
    ///
    /// Measured on MySQL 8.4.11: only the first column of an index carries a
    /// key, `UNI` when a single-column unique index makes that one column
    /// unique and `MUL` otherwise — a multi-column unique key gives its leading
    /// column `MUL`, not `UNI`. A declared `PRIMARY KEY` or `UNIQUE` outranks
    /// both, so this only fills in a column that has neither.
    fn mark_indexed_columns(
        &self,
        table: &MySqlTableName,
        metadata: &mut [MySqlColumnMetadata],
    ) -> std::result::Result<(), MySqlColumnMetadataError> {
        let indexes = self
            .list_indexes(table)
            .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?;
        let mut leading = Vec::new();
        for (position, entry) in indexes.iter().enumerate() {
            if entry.key_name() == "PRIMARY" || entry.sequence_in_index() != 1 {
                continue;
            }
            let single_column = indexes
                .get(position + 1)
                .is_none_or(|next| next.key_name() != entry.key_name());
            leading.push((entry.column_name(), entry.unique() && single_column));
        }
        for (column_name, makes_unique) in leading {
            let Some(column) = metadata
                .iter_mut()
                .find(|column| column.name.eq_ignore_ascii_case(column_name))
            else {
                continue;
            };
            let promoted = if makes_unique {
                MySqlColumnKey::Unique
            } else {
                MySqlColumnKey::Multiple
            };
            if column.key < promoted {
                column.key = promoted;
            }
        }
        Ok(())
    }

    fn list_view_columns(
        &self,
        requested_name: &str,
        catalog_name: &str,
        stored_sql: &str,
    ) -> std::result::Result<Vec<MySqlColumnMetadata>, MySqlColumnMetadataError> {
        if !catalog_name.eq_ignore_ascii_case(requested_name) {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let decoded = decode_schema_sql(SchemaSqlKind::View, stored_sql)
            .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?
            .ok_or(MySqlColumnMetadataError::UnsupportedDefinition)?;
        let mode = SessionSqlMode {
            ansi_quotes: decoded.context.sql_mode.ansi_quotes,
            no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
        };
        let statement = parse_create_view_ast(decoded.normalized_ddl, mode)
            .map_err(mysql_metadata_parse_error)?;
        let canonical = render_create_view_mysql_with_mode(&statement, mode)
            .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?;
        if canonical != decoded.normalized_ddl {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let Stmt::CreateView {
            temporary,
            if_not_exists,
            view_name,
            columns,
            select,
        } = &statement
        else {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        };
        if *temporary
            || *if_not_exists
            || view_name.db_name.is_some()
            || view_name.alias.is_some()
            || !columns.is_empty()
            || !view_name.name.as_str().eq_ignore_ascii_case(catalog_name)
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let (source_table, projected_columns) = Self::view_projection(select)?;
        if turso_core::schema::is_system_table(source_table.as_str()) {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let schema = self.inner.current_schema();
        if schema.get_view(source_table.as_str()).is_some() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        if schema.get_table(source_table.as_str()).is_none() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let core_view = schema
            .get_view(catalog_name)
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if core_view.sql != stored_sql {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let source_columns = self
            .list_columns(&source_table)
            .map_err(|error| match error {
                MySqlColumnMetadataError::TableNotFound
                | MySqlColumnMetadataError::CorruptDefinition => {
                    MySqlColumnMetadataError::CorruptDefinition
                }
                MySqlColumnMetadataError::UnsupportedDefinition => {
                    MySqlColumnMetadataError::UnsupportedDefinition
                }
                MySqlColumnMetadataError::Engine(error) => MySqlColumnMetadataError::Engine(error),
            })?;
        let mut metadata = Vec::with_capacity(projected_columns.len());
        for projected_name in projected_columns {
            if metadata.iter().any(|column: &MySqlColumnMetadata| {
                column.name.eq_ignore_ascii_case(&projected_name)
            }) {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
            let source = source_columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(&projected_name))
                .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
            let mut column = source.clone();
            column.name = projected_name;
            column.key = MySqlColumnKey::None;
            column.default_sql = None;
            column.default_value = None;
            column.extra.clear();
            metadata.push(column);
        }
        if core_view.columns.len() != metadata.len() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        for (core_column, column) in core_view.columns.iter().zip(&mut metadata) {
            if core_column.name.as_deref() != Some(column.name.as_str())
                || core_column.ty_str != column.type_name
            {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
        }
        Ok(metadata)
    }

    pub(super) fn view_projection(
        select: &turso_parser::ast::Select,
    ) -> std::result::Result<(MySqlTableName, Vec<String>), MySqlColumnMetadataError> {
        if select.with.is_some() || !select.order_by.is_empty() || select.limit.is_some() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        if !select.body.compounds.is_empty() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let OneSelect::Select {
            distinctness,
            columns,
            from,
            where_clause,
            group_by,
            window_clause,
        } = &select.body.select
        else {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if distinctness.is_some()
            || where_clause.is_some()
            || group_by.is_some()
            || !window_clause.is_empty()
        {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let Some(from) = from else {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if !from.joins.is_empty() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let SelectTable::Table(table_name, alias, indexed) = from.select.as_ref() else {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if table_name.db_name.is_some()
            || table_name.alias.is_some()
            || alias.is_some()
            || indexed.is_some()
        {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let source_table = MySqlTableName::parse(table_name.name.as_str())
            .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?;
        let mut projected_columns = Vec::with_capacity(columns.len());
        for column in columns {
            let ResultColumn::Expr(expr, alias) = column else {
                return Err(MySqlColumnMetadataError::UnsupportedDefinition);
            };
            if alias.as_ref().is_some_and(|alias| alias.is_explicit()) {
                return Err(MySqlColumnMetadataError::UnsupportedDefinition);
            }
            let projected_name = match expr.as_ref() {
                Expr::Name(name) | Expr::Id(name) => name.as_str().to_owned(),
                _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
            };
            projected_columns.push(projected_name);
        }
        if projected_columns.is_empty() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        Ok((source_table, projected_columns))
    }

    fn verify_column_indexes(
        &self,
        table_name: &str,
        columns: &[MySqlColumnMetadata],
        rowid_alias_ordinal: Option<usize>,
    ) -> std::result::Result<(), MySqlColumnMetadataError> {
        let sql = format!(
            "SELECT name FROM sqlite_schema \
             WHERE type = 'index' AND lower(tbl_name) = '{table_name}' \
             LIMIT {COLUMN_INDEX_SCAN_LIMIT}"
        );
        let rows = self
            .inner
            .prepare_internal(&sql)
            .map_err(MySqlColumnMetadataError::Engine)?
            .run_collect_rows()
            .map_err(MySqlColumnMetadataError::Engine)?;
        if Self::column_index_scan_is_truncated(rows.len()) {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        // A named index is a separate object that says nothing about how the
        // columns were declared, so it is counted out rather than refused. Only
        // the engine's own indexes have to match the inline declarations.
        let mut automatic_index_count = 0;
        for row in rows {
            let [name] = row.as_slice() else {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            };
            let name = name
                .to_text()
                .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
            if name.starts_with("sqlite_autoindex_") {
                automatic_index_count += 1;
            }
        }
        let inline_unique_count = columns
            .iter()
            .filter(|column| column.key == MySqlColumnKey::Unique)
            .count();
        let inline_primary_index_count = columns
            .iter()
            .enumerate()
            .filter(|(ordinal, column)| {
                column.key == MySqlColumnKey::Primary
                    && column.extra.is_empty()
                    && Some(*ordinal) != rowid_alias_ordinal
            })
            .count();
        if automatic_index_count != inline_unique_count + inline_primary_index_count {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        Ok(())
    }

    pub(super) fn column_index_scan_is_truncated(row_count: usize) -> bool {
        row_count == COLUMN_INDEX_SCAN_LIMIT
    }

    pub(super) fn table_list_is_truncated(row_count: usize) -> bool {
        row_count == TABLE_LIST_SCAN_LIMIT
    }
}
