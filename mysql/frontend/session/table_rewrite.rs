//! Writing a table again under a definition an `ALTER TABLE` changed: its
//! primary key, a column the key is over, the column it counts its ids on,
//! or nothing at all, as `ENGINE=InnoDB`, `FORCE` and `OPTIMIZE TABLE` ask.
//!
//! The key decides how the engine keeps the table — a key over one integer
//! is the rowid, any other is an index of its own beside one — and the
//! counted column is the rowid, so none of these can change the table in
//! place. The table is made again under a name of its own, the rows are
//! carried across in key order, the old table is dropped and the new one
//! takes its name; its indexes, its triggers and the triggers of other
//! tables that name it are written again after.

use super::*;
use turso_mysql_parser::{MySqlKeyChange, MySqlKeyRewrite};

impl MySqlConnection {
    /// An `ALTER TABLE` that changes a table's key or its counted column,
    /// read against the table it changes, and the table's name. Answers
    /// `None` for every other statement.
    pub(super) fn key_an_alter_changes(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<(String, MySqlKeyChange)>, MySqlQueryError> {
        if !sql
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("ALTER"))
        {
            return Ok(None);
        }
        let mode = self.parser_mode();
        let Some(target) = turso_mysql_parser::alter_table_target(sql, mode) else {
            return Ok(None);
        };
        let Some(stored) = self
            .stored_table_statement(&target)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(None);
        };
        Ok(
            turso_mysql_parser::table_with_its_key_changed(&stored, sql, mode)
                .map_err(mysql_query_parse_error)?
                .map(|change| (target, change)),
        )
    }

    /// Runs one `ALTER TABLE` that changes a table's key or its counted
    /// column, or answers what MySQL answers for one it refuses.
    pub(super) fn change_the_key(
        &self,
        table: &str,
        change: MySqlKeyChange,
    ) -> std::result::Result<(), MySqlQueryError> {
        let refused = |error| Err(MySqlQueryError::KeyChange(error));
        let rewrite = match change {
            MySqlKeyChange::TableWrittenAgain(rewrite) => rewrite,
            MySqlKeyChange::NoSuchColumn(name) => {
                return Err(MySqlQueryError::Engine(LimboError::NoSuchColumn { name }));
            }
            MySqlKeyChange::DuplicateColumn(name) => {
                return Err(MySqlQueryError::DuplicateColumn(name));
            }
            MySqlKeyChange::KeyColumnMissing(name) => {
                return refused(MySqlKeyChangeError::KeyColumnMissing(name));
            }
            MySqlKeyChange::NoKeyToDrop => return refused(MySqlKeyChangeError::NoKeyToDrop),
            MySqlKeyChange::SecondKey => return refused(MySqlKeyChangeError::SecondKey),
            MySqlKeyChange::KeyColumnMayBeNull => {
                return refused(MySqlKeyChangeError::KeyColumnMayBeNull);
            }
            // MySQL takes a counted column some other key starts with, which
            // the counted path here cannot keep: it wants the counter to be
            // the table's key.
            MySqlKeyChange::CountedColumnNotAKey(column) => {
                if self.inner.foreign_keys_enabled() && self.a_foreign_key_of(table, &column) {
                    return refused(MySqlKeyChangeError::ForeignKeyColumnCountingChanges);
                }
                if self.an_index_starts_with(table, &column) {
                    return Err(MySqlQueryError::Unsupported(
                        "AUTO_INCREMENT on a column a key other than the primary one starts with"
                            .to_string(),
                    ));
                }
                return refused(MySqlKeyChangeError::CountedColumnNotAKey);
            }
        };
        self.hold_the_key_change_to_the_foreign_keys(table, &rewrite)?;
        self.write_the_table_again_under(table, &rewrite)
    }

    /// Runs an `ALTER TABLE t ENGINE=InnoDB`, an `ALTER TABLE t FORCE` or an
    /// `OPTIMIZE TABLE t`, each of which MySQL answers by making the table
    /// again: measured on 8.4.11, its rows, keys, triggers and counter are as
    /// they were. Here the table is made again through the `CREATE TABLE`
    /// every table is made with now, so a table made before an integer key
    /// was the rowid takes its key as the rowid, and a plain index made
    /// before plain indexes ended with the key ends with it.
    pub fn write_the_table_again_as_it_stands(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.a_base_table_named(table)?;
        let stored = self
            .stored_table_statement(table.as_str())
            .map_err(MySqlQueryError::Engine)?
            .ok_or(MySqlQueryError::MissingTable)?;
        let rewrite = turso_mysql_parser::table_as_it_stands(&stored, self.parser_mode())
            .map_err(mysql_query_parse_error)?;
        self.write_the_table_again_under(table.as_str(), &rewrite)
    }

    /// Whether `column` is one of the columns a foreign key of `table` is
    /// over. Measured on MySQL 8.4.11, `AUTO_INCREMENT` given to one is 1832
    /// before anything else is looked at.
    fn a_foreign_key_of(&self, table: &str, column: &str) -> bool {
        self.inner
            .current_schema()
            .get_btree_table(table)
            .is_some_and(|btree| {
                btree.foreign_keys.iter().any(|foreign_key| {
                    foreign_key
                        .child_columns
                        .iter()
                        .any(|child| child.eq_ignore_ascii_case(column))
                })
            })
    }

    fn an_index_starts_with(&self, table: &str, column: &str) -> bool {
        self.inner.current_schema().get_indices(table).any(|index| {
            index
                .columns
                .first()
                .is_some_and(|first| first.name.eq_ignore_ascii_case(column))
        })
    }

    /// Refuses a key change MySQL refuses for a foreign key's sake.
    ///
    /// Measured on MySQL 8.4.11, with `fk_c1` naming `p1 (id)` from `c1
    /// (pid)`: `MODIFY id BIGINT` on `p1` and `MODIFY pid BIGINT` on `c1` are
    /// 3780 whatever `foreign_key_checks` says; `MODIFY id INT NOT NULL
    /// AUTO_INCREMENT` on `p1` is 1833 and taking `AUTO_INCREMENT` off it is
    /// too, both taken with the checks off; `AUTO_INCREMENT` on `pid` is 1832;
    /// `DROP PRIMARY KEY` on `p1`, and on a table whose own key is found only
    /// by its primary key, is 1553.
    fn hold_the_key_change_to_the_foreign_keys(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
    ) -> std::result::Result<(), MySqlQueryError> {
        let schema = self.inner.current_schema();
        let Some(btree) = schema.get_btree_table(table) else {
            return Err(MySqlQueryError::MissingTable);
        };
        let checks = self.inner.foreign_keys_enabled();
        let retyped = |column: &str| {
            rewrite
                .retyped
                .iter()
                .any(|(_, old)| old.eq_ignore_ascii_case(column))
        };
        let renamed = |column: &str| {
            rewrite
                .table
                .carried_columns
                .iter()
                .any(|(new, old)| old.eq_ignore_ascii_case(column) && new != old)
        };
        let starts_or_stops_counting = |column: &str| {
            let before = rewrite
                .counted_before
                .as_ref()
                .is_some_and(|counted| counted.eq_ignore_ascii_case(column));
            let after = rewrite.counted_after.as_ref().is_some_and(|counted| {
                rewrite.table.carried_columns.iter().any(|(new, old)| {
                    new.eq_ignore_ascii_case(counted) && old.eq_ignore_ascii_case(column)
                })
            });
            before != after
        };
        let references = schema
            .resolved_fks_referencing(table)
            .map_err(MySqlQueryError::Engine)?;
        for reference in &references {
            for (child, parent) in reference
                .fk
                .child_columns
                .iter()
                .zip(&reference.parent_cols)
            {
                if retyped(parent) {
                    return Err(MySqlQueryError::ForeignKeyDefinition(
                        MySqlForeignKeyDefinitionError::IncompatibleColumns {
                            child: child.clone(),
                            parent: parent.clone(),
                            constraint: crate::show_create_table::foreign_key_name(
                                &reference.child_table.name,
                                &reference.fk,
                                &reference.child_table.foreign_keys,
                            ),
                        },
                    ));
                }
            }
        }
        for foreign_key in &btree.foreign_keys {
            for (position, child) in foreign_key.child_columns.iter().enumerate() {
                if retyped(child) {
                    return Err(MySqlQueryError::ForeignKeyDefinition(
                        MySqlForeignKeyDefinitionError::IncompatibleColumns {
                            child: child.clone(),
                            parent: foreign_key
                                .parent_columns
                                .get(position)
                                .cloned()
                                .unwrap_or_default(),
                            constraint: crate::show_create_table::foreign_key_name(
                                table,
                                foreign_key,
                                &btree.foreign_keys,
                            ),
                        },
                    ));
                }
            }
        }
        // Measured on MySQL 8.4.11: a column a foreign key is over, on either
        // side, may be renamed only by a change MySQL makes in place; beside
        // one that copies the rows it is 1846, after 3780 and before 1833.
        let renames_a_key_column = references
            .iter()
            .any(|reference| reference.parent_cols.iter().any(|parent| renamed(parent)))
            || btree
                .foreign_keys
                .iter()
                .any(|foreign_key| foreign_key.child_columns.iter().any(|child| renamed(child)));
        if renames_a_key_column && self.the_change_copies_the_rows(table, rewrite) {
            return Err(MySqlQueryError::KeyChange(
                MySqlKeyChangeError::ForeignKeyColumnRenamedInACopy,
            ));
        }
        for reference in &references {
            if checks
                && reference
                    .parent_cols
                    .iter()
                    .any(|parent| starts_or_stops_counting(parent))
            {
                return Err(MySqlQueryError::KeyChange(
                    MySqlKeyChangeError::ReferencedColumnCountingChanges,
                ));
            }
            if !self.a_key_still_finds(table, rewrite, &reference.parent_cols, true) {
                return Err(MySqlQueryError::RequiredByForeignKey);
            }
        }
        for foreign_key in &btree.foreign_keys {
            if checks
                && foreign_key
                    .child_columns
                    .iter()
                    .any(|child| starts_or_stops_counting(child))
            {
                return Err(MySqlQueryError::KeyChange(
                    MySqlKeyChangeError::ForeignKeyColumnCountingChanges,
                ));
            }
            if !self.a_key_still_finds(table, rewrite, &foreign_key.child_columns, false) {
                return Err(MySqlQueryError::RequiredByForeignKey);
            }
        }
        Ok(())
    }

    /// Whether MySQL copies the rows to make the change: measured on 8.4.11,
    /// a column taking another type, the table starting or stopping counting,
    /// its key going with none in its place, and another collation for a
    /// column an index is over, where a collation for any other column is
    /// changed in place.
    fn the_change_copies_the_rows(&self, table: &str, rewrite: &MySqlKeyRewrite) -> bool {
        if rewrite.copies_the_rows {
            return true;
        }
        let schema = self.inner.current_schema();
        rewrite.recollated.iter().any(|recollated| {
            let Some((_, old)) = rewrite
                .table
                .carried_columns
                .iter()
                .find(|(new, _)| new.eq_ignore_ascii_case(recollated))
            else {
                return false;
            };
            schema.get_indices(table).any(|index| {
                index
                    .columns
                    .iter()
                    .any(|column| column.name.eq_ignore_ascii_case(old))
            })
        })
    }

    /// Whether, once the key has changed, some key of the table still finds
    /// the rows a foreign key names by `columns`: a parent's row by a key
    /// over exactly those columns, a child's by one they begin.
    fn a_key_still_finds(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
        columns: &[String],
        unique: bool,
    ) -> bool {
        let new_name = |old: &String| {
            rewrite
                .table
                .carried_columns
                .iter()
                .find(|(_, carried)| carried.eq_ignore_ascii_case(old))
                .map(|(new, _)| new.clone())
                .unwrap_or_else(|| old.clone())
        };
        let columns = columns.iter().map(new_name).collect::<Vec<_>>();
        let serves = |key: &[String]| {
            if unique {
                key.len() == columns.len()
                    && columns
                        .iter()
                        .all(|column| key.iter().any(|part| part.eq_ignore_ascii_case(column)))
            } else {
                key.len() >= columns.len()
                    && columns
                        .iter()
                        .zip(key)
                        .all(|(column, part)| part.eq_ignore_ascii_case(column))
            }
        };
        if serves(&rewrite.new_key) {
            return true;
        }
        let schema = self.inner.current_schema();
        let primary_key = schema
            .get_btree_table(table)
            .map(|btree| btree.primary_key_columns.clone())
            .unwrap_or_default();
        let served = schema.get_indices(table).any(|index| {
            !is_the_primary_keys_own_index(index, &primary_key)
                && (index.unique || !unique)
                && serves(
                    &mysql_index_columns(index, &primary_key)
                        .iter()
                        .map(|column| new_name(&column.name))
                        .collect::<Vec<_>>(),
                )
        });
        served
    }

    /// Makes the table again under the definition `rewrite` carries and
    /// carries its rows across, all in one transaction.
    fn write_the_table_again_under(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
    ) -> std::result::Result<(), MySqlQueryError> {
        let counter_before = self
            .counter_of_a_stored_table(table)
            .map_err(MySqlQueryError::Engine)?;
        let indexes = self
            .stored_index_statements(table)
            .map_err(MySqlQueryError::Engine)?
            .into_iter()
            .map(|index| self.index_over_the_carried_columns(index, rewrite))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let triggers = self
            .triggers_naming(table)
            .map_err(MySqlQueryError::Engine)?;
        let referencing = self.keys_naming_renamed_columns(table, rewrite)?;
        let made_new = format!("{table}_turso_written");
        let written_prefix = format!("CREATE TABLE {} (", mysql_quoted(table));
        let Some(columns_onwards) = rewrite.table.create_sql.strip_prefix(&written_prefix) else {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "a table written again does not begin with its name".to_string(),
            )));
        };
        let create = format!(
            "CREATE TABLE {} ({columns_onwards}",
            mysql_quoted(&made_new)
        );
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        self.run_internal("BEGIN")?;
        let checks = self.inner.foreign_keys_enabled();
        self.set_foreign_key_checks(false);
        let written = self.write_the_table_again_in_this_transaction(
            table,
            rewrite,
            &made_new,
            &create,
            counter_before,
            &indexes,
            &triggers,
            &referencing,
        );
        self.set_foreign_key_checks(checks);
        if let Err(error) = written {
            self.run_internal("ROLLBACK")?;
            return Err(error);
        }
        self.run_internal("COMMIT")?;
        crash_point(CrashPoint::SchemaChangeCommitted);
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")?;
        }
        Ok(())
    }

    /// The statements a table's rewrite is made of.
    ///
    /// The table written again takes a counter of its own, which starts at
    /// one, so it is moved past every id the rows hold before the
    /// transaction commits: a crash after the commit then finds the counter
    /// already there, and one before it finds the old table and its counter.
    #[allow(clippy::too_many_arguments)]
    fn write_the_table_again_in_this_transaction(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
        made_new: &str,
        create: &str,
        counter_before: Option<u64>,
        indexes: &[StoredIndexStatement],
        triggers: &[StoredTrigger],
        referencing: &[ReferencingKey],
    ) -> std::result::Result<(), MySqlQueryError> {
        for trigger in triggers {
            self.drop_a_trigger_to_write_again(&trigger.name)?;
        }
        self.refuse_nulls_in_the_new_key(table, rewrite)?;
        let numbered = match &rewrite.counted_after {
            Some(counted) => Some(self.number_the_rows_asking_for_one(
                table,
                rewrite,
                counted,
                counter_before.filter(|_| rewrite.counted_before.is_some()),
            )?),
            None => None,
        };
        self.prepare(create)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(|error| self.json_schema_prepare_error(create, error))?;
        if let Some(counted) = &rewrite.counted_after {
            self.hold_the_counted_ids_to_their_column(table, made_new, rewrite, counted)?;
        }
        // Measured on MySQL 8.4.11: a column made `NOT NULL` over a row
        // holding NULL is 1138, as a key added over one is.
        self.carry_the_rows_across(&self.copy_in_key_order(table, made_new, rewrite), made_new)
            .map_err(|error| match error {
                MySqlQueryError::Engine(LimboError::NotNullConstraint { .. }) => {
                    MySqlQueryError::KeyChange(MySqlKeyChangeError::NullInANotNullColumn)
                }
                MySqlQueryError::Engine(LimboError::Assignment(cut))
                    if self.cuts_a_word_kept_as_one(made_new, rewrite, &cut) =>
                {
                    MySqlQueryError::KeyChange(MySqlKeyChangeError::WordCutShort)
                }
                error => error,
            })?;
        self.run_internal(&format!("DROP TABLE {}", sqlite_quoted(table)))?;
        self.rename_the_table_written_again(made_new, table)?;
        for index in indexes {
            self.prepare_with_index_origin(&index.sql, index.implicit)
                .and_then(|mut prepared| prepared.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        // Measured on MySQL 8.4.11: the index made for a foreign key goes once
        // a primary key added over its columns finds the rows instead.
        let named = MySqlTableName::parse(table)
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        self.remove_replaced_implicit_fk_indexes(&named)?;
        for key in referencing {
            self.point_a_key_at_the_renamed_columns(key)?;
        }
        for trigger in triggers {
            self.write_a_trigger_again(trigger)?;
        }
        let Some(numbered) = numbered else {
            return Ok(());
        };
        // Measured on MySQL 8.4.11: a table that counted before counts on
        // from where it stood, `AUTO_INCREMENT=100` kept over rows reaching
        // 11, and one that starts counting goes on from its highest id.
        let highest = self.highest_id_held(table, rewrite)?;
        let kept = counter_before
            .filter(|_| rewrite.counted_before.is_some())
            .unwrap_or(0);
        self.move_the_new_counter_past(table, numbered.max(highest).max(kept))
    }

    pub(super) fn move_the_new_counter_past(
        &self,
        table: &str,
        high_water: u64,
    ) -> std::result::Result<(), MySqlQueryError> {
        if high_water == 0 {
            return Ok(());
        }
        let Some(counted) = self
            .load_auto_increment_table(table)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "a table written again to count stopped counting".to_string(),
            )));
        };
        self.advance_auto_increment_past(&counted, high_water, None)
    }

    /// The foreign keys of other tables naming a column the statement
    /// renames, each as it is to read once the table is written again.
    ///
    /// Measured on MySQL 8.4.11: after `CHANGE id pk INT NOT NULL` on a
    /// table another table's `fk_c` names by `id`, `SHOW CREATE TABLE` of the
    /// other table prints ``REFERENCES `p` (`pk`)`` with its actions as they
    /// were, `KEY_COLUMN_USAGE` names `pk` as the referenced column, and the
    /// key is held as before.
    fn keys_naming_renamed_columns(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
    ) -> std::result::Result<Vec<ReferencingKey>, MySqlQueryError> {
        let renamed = |column: &str| {
            rewrite
                .table
                .carried_columns
                .iter()
                .find(|(new, old)| new != old && old.eq_ignore_ascii_case(column))
                .map(|(new, _)| new.clone())
        };
        let schema = self.inner.current_schema();
        let references = schema
            .resolved_fks_referencing(table)
            .map_err(MySqlQueryError::Engine)?;
        let mut children: Vec<String> = Vec::new();
        for reference in &references {
            let child = &reference.child_table.name;
            if child.eq_ignore_ascii_case(table)
                || children
                    .iter()
                    .any(|named| named.eq_ignore_ascii_case(child))
                || !reference
                    .parent_cols
                    .iter()
                    .any(|parent| renamed(parent).is_some())
            {
                continue;
            }
            children.push(child.clone());
        }
        let mut keys = Vec::new();
        for child in children {
            let (Some(stored), Some(btree)) =
                (schema.table_sql(&child), schema.get_btree_table(&child))
            else {
                return Err(MySqlQueryError::MissingTable);
            };
            let names = btree
                .foreign_keys
                .iter()
                .map(|foreign_key| {
                    crate::show_create_table::foreign_key_name(
                        &child,
                        foreign_key,
                        &btree.foreign_keys,
                    )
                })
                .collect::<Vec<_>>();
            let statement = self
                .inner
                .dialect()
                .parse_schema_sql(SchemaSqlKind::Table, stored)
                .map_err(MySqlQueryError::Engine)?;
            let Stmt::CreateTable {
                body: turso_parser::ast::CreateTableBody::ColumnsAndConstraints { constraints, .. },
                ..
            } = statement
            else {
                return Err(MySqlQueryError::Engine(LimboError::Corrupt(
                    "a stored table did not describe its columns".to_string(),
                )));
            };
            let mut unnamed = 0;
            for named in constraints {
                let turso_parser::ast::TableConstraint::ForeignKey { clause, .. } =
                    &named.constraint
                else {
                    continue;
                };
                let name = match &named.name {
                    Some(name) => name.as_str().to_owned(),
                    None => {
                        unnamed += 1;
                        format!("{child}_ibfk_{unnamed}")
                    }
                };
                if !clause.tbl_name.as_str().eq_ignore_ascii_case(table) {
                    continue;
                }
                let mut constraint = named.constraint.clone();
                let turso_parser::ast::TableConstraint::ForeignKey { clause, .. } = &mut constraint
                else {
                    unreachable!("the constraint was read as a foreign key above");
                };
                let mut points_elsewhere = false;
                for column in &mut clause.columns {
                    if let Some(new) = renamed(column.col_name.as_str()) {
                        column.col_name = turso_parser::ast::Name::exact(new);
                        points_elsewhere = true;
                    }
                }
                if !points_elsewhere {
                    continue;
                }
                if !names.iter().any(|known| known.eq_ignore_ascii_case(&name)) {
                    return Err(MySqlQueryError::Unsupported(
                        "a foreign key whose stored place does not give its name".to_string(),
                    ));
                }
                keys.push(ReferencingKey {
                    table: child.clone(),
                    name: name.clone(),
                    constraint: turso_parser::ast::NamedTableConstraint {
                        name: Some(turso_parser::ast::Name::exact(name)),
                        constraint,
                    },
                });
            }
        }
        Ok(keys)
    }

    /// Points one foreign key of another table at the columns it names under
    /// their new names, by dropping it and adding it again under the name it
    /// answers to; the engine's own `DROP CONSTRAINT` keeps every other key
    /// of the table under the name it answers to as well.
    fn point_a_key_at_the_renamed_columns(
        &self,
        key: &ReferencingKey,
    ) -> std::result::Result<(), MySqlQueryError> {
        for body in [
            AlterTableBody::DropConstraint(turso_parser::ast::Name::exact(key.name.clone())),
            AlterTableBody::AddConstraint(key.constraint.clone()),
        ] {
            let statement = Stmt::AlterTable(turso_parser::ast::AlterTable {
                name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                    key.table.clone(),
                )),
                body,
            });
            let sql = statement.to_string();
            let options =
                PrepareOptions::default().with_schema_sql_formatter(Arc::new(self.schema_context));
            self.inner
                .prepare_translated_stmt_with_options(statement, &sql, &options)
                .and_then(|mut prepared| prepared.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        Ok(())
    }

    /// Whether a value too long for its column was copied from a column of
    /// words into one that still holds words.
    fn cuts_a_word_kept_as_one(
        &self,
        made_new: &str,
        rewrite: &MySqlKeyRewrite,
        error: &turso_core::AssignmentError,
    ) -> bool {
        let turso_core::AssignmentError::TooLong { column, .. } = error else {
            return false;
        };
        let schema = self.inner.current_schema();
        let Some(name) = schema
            .get_btree_table(made_new)
            .and_then(|btree| btree.columns().get(column.saturating_sub(1)).cloned())
            .and_then(|written| written.name)
        else {
            return false;
        };
        rewrite
            .words_kept_as_words
            .iter()
            .any(|kept| kept.eq_ignore_ascii_case(&name))
    }

    /// One stored index statement, written over the names its columns have
    /// once the statement renamed them.
    fn index_over_the_carried_columns(
        &self,
        index: StoredIndexStatement,
        rewrite: &MySqlKeyRewrite,
    ) -> std::result::Result<StoredIndexStatement, MySqlQueryError> {
        let renames = rewrite
            .table
            .carried_columns
            .iter()
            .filter(|(new, old)| new != old)
            .collect::<Vec<_>>();
        if renames.is_empty() {
            return Ok(index);
        }
        let mode = self.parser_mode();
        let mut statement =
            parse_schema_ddl_ast(&index.sql, mode).map_err(mysql_query_parse_error)?;
        let Stmt::CreateIndex { columns, .. } = &mut statement else {
            return Err(MySqlQueryError::Engine(LimboError::Corrupt(
                "a stored index statement did not describe an index".to_string(),
            )));
        };
        for column in columns.iter_mut() {
            let Expr::Id(name) = column.expr.as_mut() else {
                continue;
            };
            if let Some((new, _)) = renames
                .iter()
                .find(|(_, old)| old.eq_ignore_ascii_case(name.as_str()))
            {
                *name = turso_parser::ast::Name::exact(new.clone());
            }
        }
        Ok(StoredIndexStatement {
            sql: render_create_index_mysql_with_mode(&statement, mode)
                .map_err(mysql_query_parse_error)?,
            ..index
        })
    }

    /// The triggers to write again once the table is: its own, which go with
    /// the table it is set on, and every other one whose text names it, which
    /// the engine would refuse to leave naming a table that is not there
    /// while the new one takes the name.
    fn triggers_naming(&self, table: &str) -> Result<Vec<StoredTrigger>> {
        let rows = self
            .inner
            .prepare("SELECT name, tbl_name, sql FROM sqlite_schema WHERE type = 'trigger'")?
            .run_collect_rows()?;
        let lowered = table.to_ascii_lowercase();
        let mut triggers = Vec::new();
        for row in rows {
            let [name, owner, sql] = row.as_slice() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema trigger row has an invalid shape".to_string(),
                ));
            };
            let (Some(name), Some(owner), Some(sql)) =
                (name.to_text(), owner.to_text(), sql.to_text())
            else {
                return Err(LimboError::InternalError(
                    "sqlite_schema trigger row holds something other than text".to_string(),
                ));
            };
            let names_the_table =
                owner.eq_ignore_ascii_case(table) || sql.to_ascii_lowercase().contains(&lowered);
            if names_the_table {
                triggers.push(StoredTrigger {
                    name: name.to_owned(),
                    stored_sql: sql.to_owned(),
                });
            }
        }
        Ok(triggers)
    }

    fn drop_a_trigger_to_write_again(
        &self,
        name: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let statement = Stmt::DropTrigger {
            if_exists: false,
            trigger_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                name.to_owned(),
            )),
        };
        self.inner
            .prepare_translated_stmt(statement, &format!("DROP TRIGGER {}", sqlite_quoted(name)))
            .and_then(|mut prepared| prepared.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
    }

    /// Makes one trigger again exactly as it was stored: its definer, its
    /// `sql_mode`, the moment it was made and its text all come back.
    fn write_a_trigger_again(
        &self,
        trigger: &StoredTrigger,
    ) -> std::result::Result<(), MySqlQueryError> {
        let statement = self
            .inner
            .dialect()
            .parse_schema_sql(SchemaSqlKind::Trigger, &trigger.stored_sql)
            .map_err(MySqlQueryError::Engine)?;
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenTransactionStatementParser {
                statement: statement.clone(),
            }))
            .with_schema_sql_formatter(Arc::new(StoredSchemaSqlFormatter {
                context: self.schema_context,
                kind: SchemaSqlKind::Trigger,
                stored_sql: trigger.stored_sql.clone(),
            }));
        self.inner
            .prepare_translated_stmt_with_options(statement, &trigger.stored_sql, &options)
            .and_then(|mut prepared| prepared.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
    }

    /// Refuses a key over a column holding NULL in some row. Measured on
    /// MySQL 8.4.11: `ADD PRIMARY KEY (id)` over a row whose `id` is NULL is
    /// 1138, and nothing changes. A column the table starts counting on is
    /// left out: its NULLs ask for the next number.
    fn refuse_nulls_in_the_new_key(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
    ) -> std::result::Result<(), MySqlQueryError> {
        for column in &rewrite.new_key {
            if rewrite
                .counted_after
                .as_ref()
                .is_some_and(|counted| counted.eq_ignore_ascii_case(column))
            {
                continue;
            }
            let Some((_, old)) = rewrite
                .table
                .carried_columns
                .iter()
                .find(|(new, _)| new.eq_ignore_ascii_case(column))
            else {
                continue;
            };
            let sql = format!(
                "SELECT 1 FROM {} WHERE {} IS NULL LIMIT 1",
                sqlite_quoted(table),
                sqlite_quoted(old)
            );
            let rows = self
                .inner
                .prepare(sql)
                .and_then(|mut statement| statement.run_collect_rows())
                .map_err(MySqlQueryError::Engine)?;
            if !rows.is_empty() {
                return Err(MySqlQueryError::KeyChange(
                    MySqlKeyChangeError::NullInANotNullColumn,
                ));
            }
        }
        Ok(())
    }

    /// Numbers each row whose counted column asks for a number, the way
    /// MySQL numbers them as it copies the rows, and answers the highest
    /// number the copy spent.
    ///
    /// Measured on MySQL 8.4.11: the rows go across in the order of the old
    /// table's key, or the order they were written where it had none; a NULL,
    /// and a 0 unless `sql_mode` names `NO_AUTO_VALUE_ON_ZERO`, takes the next
    /// number, which is one past the counter the table had, or one past the
    /// highest id copied before it; and the first such row reserves as many
    /// numbers as the table has rows, so over rows `5, NULL, 0, NULL, 3, 0`
    /// they take 6, 7, 8 and 9 and the table then reads `AUTO_INCREMENT=12`.
    /// A number taken already is 1062, and nothing changes.
    fn number_the_rows_asking_for_one(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
        counted: &str,
        counter_before: Option<u64>,
    ) -> std::result::Result<u64, MySqlQueryError> {
        let Some((_, old)) = rewrite
            .table
            .carried_columns
            .iter()
            .find(|(new, _)| new.eq_ignore_ascii_case(counted))
        else {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "the counted column is not one of the carried columns".to_string(),
            )));
        };
        let order = if rewrite.old_key.is_empty() {
            "rowid".to_string()
        } else {
            rewrite
                .old_key
                .iter()
                .map(|column| sqlite_quoted(column))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let sql = format!(
            "SELECT rowid, {} FROM {} ORDER BY {order}",
            sqlite_quoted(old),
            sqlite_quoted(table)
        );
        let rows = self
            .inner
            .prepare(sql)
            .and_then(|mut statement| statement.run_collect_rows())
            .map_err(MySqlQueryError::Engine)?;
        let asks_for_zero = self.written_zero() == WrittenZero::AsksForTheNextNumber;
        let mut next = counter_before.unwrap_or(0).saturating_add(1);
        let mut reserved_end: Option<u64> = None;
        let mut reservations = 0_u32;
        let mut numbered = Vec::new();
        for row in &rows {
            let [rowid, value] = row.as_slice() else {
                return Err(MySqlQueryError::Engine(LimboError::InternalError(
                    "a numbered row has an invalid shape".to_string(),
                )));
            };
            let asks = match value {
                Value::Null => true,
                value => asks_for_zero && value.as_int() == Some(0),
            };
            if !asks {
                if let Some(id) = value.as_int().filter(|id| *id > 0) {
                    next = next.max(id as u64 + 1);
                }
                continue;
            }
            if reserved_end.is_none_or(|end| next > end) {
                // MySQL asks for as many numbers as the table holds rows at
                // first, and then for 1, 2, 4 and on up.
                let wanted = if reservations == 0 {
                    rows.len() as u64
                } else {
                    (1_u64 << reservations.min(16)).min(65_535)
                };
                reservations += 1;
                reserved_end = Some(next + wanted - 1);
            }
            numbered.push((rowid.clone(), next));
            next += 1;
        }
        for (rowid, id) in &numbered {
            let sql = format!(
                "UPDATE {} SET {} = {id} WHERE rowid = {}",
                sqlite_quoted(table),
                sqlite_quoted(old),
                rowid
            );
            self.run_internal(&sql)?;
        }
        Ok(reserved_end.unwrap_or(0))
    }

    /// Refuses an id the counted column's new type cannot hold, which the
    /// copy into the new table's rowid would not.
    fn hold_the_counted_ids_to_their_column(
        &self,
        table: &str,
        made_new: &str,
        rewrite: &MySqlKeyRewrite,
        counted: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let Some(counted_table) = self
            .load_auto_increment_table(made_new)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "a table written again to count does not count".to_string(),
            )));
        };
        let Some((_, old)) = rewrite
            .table
            .carried_columns
            .iter()
            .find(|(new, _)| new.eq_ignore_ascii_case(counted))
        else {
            return Ok(());
        };
        // A `BIGINT UNSIGNED` counter keeps its ids in a column of their own,
        // which the copy's validator holds to the type as it does any other.
        if counted_table.definition.allocator_column_type
            == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
        {
            return Ok(());
        }
        let (least, most) = counted_table.definition.allocator_column_type.bounds();
        let sql = format!(
            "SELECT {old} FROM {table} WHERE typeof({old}) <> 'integer' OR {old} < {least} OR {old} > {most}",
            old = sqlite_quoted(old),
            table = sqlite_quoted(table),
        );
        let rows = self
            .inner
            .prepare(sql)
            .and_then(|mut statement| statement.run_collect_rows())
            .map_err(MySqlQueryError::Engine)?;
        for row in rows {
            let id = match row.first() {
                Some(Value::Numeric(Numeric::Integer(id))) => Some(i128::from(*id)),
                Some(Value::Text(text)) => text.as_str().trim().parse::<i128>().ok(),
                _ => None,
            };
            match id {
                Some(id) => hold_the_id_to_the_counted_column(&counted_table, id)
                    .map_err(MySqlQueryError::Engine)?,
                None => {
                    return Err(MySqlQueryError::Engine(LimboError::from(
                        turso_core::AssignmentError::IncorrectType {
                            table: table.to_owned(),
                            column: counted_table.definition.allocator_column_ordinal + 1,
                            type_name: counted_table
                                .definition
                                .allocator_column_written_type
                                .to_string(),
                        },
                    )));
                }
            }
        }
        Ok(())
    }

    /// The `INSERT ... SELECT` carrying a table's rows into the one written
    /// again, in the order of its key: the new key where it has one, so a
    /// key kept as an index of its own reads back in key order as InnoDB's
    /// does, and otherwise the old one.
    fn copy_in_key_order(&self, table: &str, made_new: &str, rewrite: &MySqlKeyRewrite) -> String {
        let written_into = rewrite
            .table
            .carried_columns
            .iter()
            .map(|(column, _)| sqlite_quoted(column))
            .collect::<Vec<_>>()
            .join(", ");
        let read_from = rewrite
            .table
            .carried_columns
            .iter()
            .map(|(_, column)| sqlite_quoted(column))
            .collect::<Vec<_>>()
            .join(", ");
        let old_name = |new: &String| {
            rewrite
                .table
                .carried_columns
                .iter()
                .find(|(carried, _)| carried.eq_ignore_ascii_case(new))
                .map(|(_, old)| old.clone())
        };
        let order = rewrite
            .new_key
            .iter()
            .map(old_name)
            .collect::<Option<Vec<_>>>()
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| rewrite.old_key.clone());
        let order_by = if order.is_empty() {
            String::new()
        } else {
            format!(
                " ORDER BY {}",
                order
                    .iter()
                    .map(|column| sqlite_quoted(column))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "INSERT INTO {} ({written_into}) SELECT {read_from} FROM {}{order_by}",
            sqlite_quoted(made_new),
            sqlite_quoted(table)
        )
    }

    /// Gives the table written again the name of the one it replaces.
    ///
    /// The engine's rename is asked for directly: nothing else names the new
    /// table's own name, so no view or trigger has text the rename changes.
    fn rename_the_table_written_again(
        &self,
        made_new: &str,
        table: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let statement = Stmt::AlterTable(turso_parser::ast::AlterTable {
            name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                made_new.to_owned(),
            )),
            body: AlterTableBody::RenameTo(turso_parser::ast::Name::exact(table.to_owned())),
        });
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenSchemaDdlParser {
                mode: self.parser_mode(),
            }))
            .with_schema_sql_formatter(Arc::new(self.schema_context));
        self.inner
            .prepare_translated_stmt_with_options(
                statement,
                &format!(
                    "ALTER TABLE {} RENAME TO {}",
                    mysql_quoted(made_new),
                    mysql_quoted(table)
                ),
                &options,
            )
            .and_then(|mut prepared| prepared.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
    }

    fn highest_id_held(
        &self,
        table: &str,
        rewrite: &MySqlKeyRewrite,
    ) -> std::result::Result<u64, MySqlQueryError> {
        let Some(counted) = &rewrite.counted_after else {
            return Ok(0);
        };
        let sql = format!(
            "SELECT MAX({}) FROM {}",
            sqlite_quoted(counted),
            sqlite_quoted(table)
        );
        let rows = self
            .inner
            .prepare(sql)
            .and_then(|mut statement| statement.run_collect_rows())
            .map_err(MySqlQueryError::Engine)?;
        Ok(rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_int)
            .and_then(|id| u64::try_from(id).ok())
            .unwrap_or(0))
    }
}

/// A foreign key of another table, as it is to read once a column it names
/// is renamed.
pub(super) struct ReferencingKey {
    table: String,
    name: String,
    constraint: turso_parser::ast::NamedTableConstraint,
}

/// One trigger's row of `sqlite_schema`, kept to be written again as it was.
pub(super) struct StoredTrigger {
    name: String,
    stored_sql: String,
}

/// Writes one schema row back exactly as it was stored.
struct StoredSchemaSqlFormatter {
    context: SchemaSqlSessionContext,
    kind: SchemaSqlKind,
    stored_sql: String,
}

impl SchemaSqlFormatter for StoredSchemaSqlFormatter {
    fn format_schema_sql(&self, kind: SchemaSqlKind, input: &str, stmt: &Stmt) -> Result<String> {
        if kind == self.kind {
            return Ok(self.stored_sql.clone());
        }
        self.context.format_schema_sql(kind, input, stmt)
    }

    fn format_rewritten_schema_sql(
        &self,
        kind: SchemaSqlKind,
        previous_sql: &str,
        stmt: &Stmt,
    ) -> Result<String> {
        self.context
            .format_rewritten_schema_sql(kind, previous_sql, stmt)
    }
}
