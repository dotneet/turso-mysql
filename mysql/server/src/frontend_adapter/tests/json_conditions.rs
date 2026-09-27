//! Conditions over a `JSON` column, as the frameworks write them: Laravel's
//! `where('meta->lang', 'en')` is `json_unquote(json_extract(meta,
//! '$."lang"')) = ?`, its `whereJsonContains` a bare `json_contains(...)`, its
//! `whereNull('meta->x')` a `json_extract(...) is null OR json_type(...) =
//! 'NULL'`, and Rails writes `meta->>'$.lang' = 'en'` by hand.
//!
//! Every expectation was measured on MySQL 8.4.11 over the same seven rows.

use super::*;

const ROWS: [&str; 7] = [
    r#"(1, '{"a":"1","lang":"en","tags":["x","y"],"n":1,"f":1.5,"b":{"c":"deep"}}')"#,
    r#"(2, '{"a":1,"lang":"EN","tags":["y"],"n":"1","f":"1.50","nul":null}')"#,
    r#"(3, '{"a":"en ","lang":"en ","n":10,"arr":[1,2,3]}')"#,
    r#"(4, '[1,"two",{"a":3}]')"#,
    "(5, NULL)",
    r#"(6, '"scalar"')"#,
    r#"(7, '{"lang":"éa","a":true}')"#,
];

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([151; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE items (id INT PRIMARY KEY, doc JSON, seen INT, note TEXT)")
        .unwrap();
    for row in ROWS {
        let sql = format!("INSERT INTO items (id, doc) VALUES {row}");
        adapter
            .execute_query(&sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn ids(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    condition: &str,
) -> Vec<u32> {
    let sql = format!("SELECT id FROM items WHERE {condition} ORDER BY id");
    let result = adapter
        .execute_query(&sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    let CommandExecutionResult::ResultSet(result) = result else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            String::from_utf8(row[0].clone().unwrap())
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect()
}

fn assert_ids(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    measured: &[(&str, &[u32])],
) {
    for (condition, expected) in measured {
        assert_eq!(ids(adapter, condition), *expected, "{condition}");
    }
}

/// The text `->>` and `JSON_UNQUOTE(JSON_EXTRACT(...))` answer carries
/// `utf8mb4_bin`: it tells `en` from `EN` and pads with spaces. Against a
/// number both sides are read as doubles, the text by the number it begins
/// with. The JSON null unquotes to the word `null`.
#[test]
fn unquoted_json_text_compares_the_way_mysql_compares_it() {
    let (_directory, mut adapter) = adapter();
    assert_ids(
        &mut adapter,
        &[
            (
                r#"json_unquote(json_extract(`doc`, '$."lang"')) = 'en'"#,
                &[1, 3],
            ),
            (
                r#"json_unquote(json_extract(`doc`, '$."lang"')) = 'EN'"#,
                &[2],
            ),
            ("JSON_UNQUOTE(JSON_EXTRACT(doc, '$.lang')) = 'en'", &[1, 3]),
            ("doc->>'$.lang' = 'en'", &[1, 3]),
            ("doc->>'$.lang' = 'en  '", &[1, 3]),
            ("doc->>'$.lang' = 'ea'", &[]),
            ("doc->>'$.lang' = 'éa'", &[7]),
            ("doc->>'$.lang' < 'f'", &[1, 2, 3]),
            ("doc->>'$.lang' <> 'en'", &[2, 7]),
            ("doc->>'$.lang' <=> 'en'", &[1, 3]),
            ("doc->>'$.lang' = NULL", &[]),
            ("'en' = doc->>'$.lang'", &[1, 3]),
            (r#"doc->>'$."lang"' = 'en'"#, &[1, 3]),
            ("doc->>'$.a' = '1'", &[1, 2]),
            ("doc->>'$.a' = '1.0'", &[]),
            ("doc->>'$.a' = 1", &[1, 2]),
            ("doc->>'$.a' = 1.0", &[1, 2]),
            ("doc->>'$.a' > 1e0", &[]),
            ("doc->>'$.a' <> 1", &[3, 7]),
            ("doc->>'$.a' <=> 1", &[1, 2]),
            ("doc->>'$.a' <=> NULL", &[4, 5, 6]),
            ("doc->>'$.a' = 'true'", &[7]),
            ("doc->>'$.a' = TRUE", &[1, 2]),
            ("doc->>'$.a' = -1.5e0", &[]),
            ("doc->>'$.n' >= 10", &[3]),
            ("doc->>'$.lang' = 0", &[1, 2, 3, 7]),
            ("doc->>'$.n' = 1", &[1, 2]),
            ("doc->>'$.n' > 2", &[3]),
            ("doc->>'$.n' > '2'", &[]),
            ("doc->>'$.f' = 1.5", &[1, 2]),
            ("doc->>'$.nul' = 'null'", &[2]),
            ("doc->>'$.nul' IS NULL", &[1, 3, 4, 5, 6, 7]),
            ("doc->>'$.missing' IS NULL", &[1, 2, 3, 4, 5, 6, 7]),
            ("doc->>'$.b.c' = 'deep'", &[1]),
            (r#"doc->>'$.b."c"' = 'deep'"#, &[1]),
            ("doc->>'$[1]' = 'two'", &[4]),
            ("doc->>'$.tags[0]' = 'x'", &[1]),
            (r#"doc->>'$.tags' = '["x", "y"]'"#, &[1]),
            ("doc->>'$' = 'scalar'", &[6]),
            // MySQL reads a lone value as an array of one, so `[0]` over an
            // object is the object.
            ("doc->>'$[0].lang' = 'en'", &[1, 3]),
        ],
    );
}

/// `->` and `JSON_EXTRACT` answer a JSON value, which MySQL compares by
/// JSON's rules: a word only meets a JSON string, byte for byte, a number
/// only a JSON number, and a string ranks above every number.
#[test]
fn a_json_value_read_out_compares_by_json_rules() {
    let (_directory, mut adapter) = adapter();
    assert_ids(
        &mut adapter,
        &[
            ("JSON_EXTRACT(doc, '$.lang') = 'en'", &[1]),
            ("JSON_EXTRACT(doc, '$.a') = 1", &[2]),
            ("JSON_EXTRACT(doc, '$.a') = '1'", &[1]),
            ("doc->'$.a' = 1", &[2]),
            ("doc->'$.a' = '1'", &[1]),
            ("doc->'$.lang' = 'EN'", &[2]),
            ("doc->'$.lang' = 'en '", &[3]),
            ("doc->'$.lang' > 'e'", &[1, 3, 7]),
            ("doc->'$.n' > 1", &[2, 3]),
            ("doc->'$.n' > '1'", &[]),
            (r#"doc->'$.tags' = '["x", "y"]'"#, &[]),
            ("doc->'$.nul' = 'null'", &[]),
            ("doc->'$.nul' IS NULL", &[1, 3, 4, 5, 6, 7]),
            ("doc->'$.a' <=> NULL", &[4, 5, 6]),
            ("doc->'$[0]' = 1", &[4]),
            ("json_extract(doc, '$.lang') IS NOT NULL", &[1, 2, 3, 7]),
        ],
    );
}

/// Laravel's `whereNull('meta->x')` and `whereNotNull('meta->x')`: a member
/// holding the JSON null is found by `JSON_EXTRACT`, and `JSON_TYPE` names it
/// `NULL` under `utf8mb4_bin`.
#[test]
fn the_null_tests_laravel_writes_find_what_mysql_finds() {
    let (_directory, mut adapter) = adapter();
    assert_ids(
        &mut adapter,
        &[
            (
                r#"(json_extract(`doc`, '$."nul"') is null OR json_type(json_extract(`doc`, '$."nul"')) = 'NULL')"#,
                &[1, 2, 3, 4, 5, 6, 7],
            ),
            (
                r#"(json_extract(`doc`, '$."nul"') is not null AND json_type(json_extract(`doc`, '$."nul"')) != 'NULL')"#,
                &[],
            ),
            ("json_type(doc->'$.nul') = 'null'", &[]),
            ("json_type(doc) = 'OBJECT'", &[1, 2, 3, 7]),
            ("json_type(doc->'$.a') <> 'STRING'", &[2, 7]),
        ],
    );
}

/// `JSON_CONTAINS`, `JSON_CONTAINS_PATH` under Laravel's `ifnull(..., 0)`, and
/// `JSON_LENGTH`, standing on their own or compared with a number.
#[test]
fn containment_paths_and_lengths_find_what_mysql_finds() {
    let (_directory, mut adapter) = adapter();
    assert_ids(
        &mut adapter,
        &[
            (r#"json_contains(`doc`, '"x"', '$."tags"')"#, &[1]),
            (r#"not json_contains(`doc`, '"x"', '$."tags"')"#, &[2]),
            (r#"json_contains(doc, '{"lang":"en"}')"#, &[1]),
            ("json_contains(doc, '1', '$.a')", &[2]),
            (
                r#"ifnull(json_contains_path(`doc`, 'one', '$."nul"'), 0)"#,
                &[2],
            ),
            (
                r#"not ifnull(json_contains_path(`doc`, 'one', '$."nul"'), 0)"#,
                &[1, 3, 4, 5, 6, 7],
            ),
            (
                "json_contains_path(doc, 'all', '$.a', '$.lang')",
                &[1, 2, 3, 7],
            ),
            ("json_contains_path(doc, 'ONE', '$.b.c', '$.arr')", &[1, 3]),
            (r#"json_length(`doc`, '$."tags"') = 2"#, &[1]),
            (r#"json_length(`doc`, '$."tags"') >= 1"#, &[1, 2]),
            ("json_length(doc) > 5", &[1, 2]),
            ("json_length(doc) = TRUE", &[6]),
            ("json_length(doc, '$.lang') = 1", &[1, 2, 3, 7]),
            ("json_length(doc->'$.tags') = 1", &[2]),
        ],
    );
}

/// An `UPDATE` or a `DELETE` names its rows by the same conditions.
#[test]
fn a_write_names_its_rows_by_a_json_condition() {
    let (_directory, mut adapter) = adapter();
    let affected = |adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
                    sql: &str| match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(
        affected(
            &mut adapter,
            "UPDATE items SET seen = 1 WHERE doc->>'$.lang' = 'en'"
        ),
        2
    );
    assert_ids(&mut adapter, &[("seen = 1", &[1, 3])]);
    assert_eq!(
        affected(
            &mut adapter,
            r#"DELETE FROM items WHERE json_contains(doc, '"y"', '$.tags')"#
        ),
        2
    );
    assert_eq!(
        affected(
            &mut adapter,
            "DELETE FROM items WHERE ifnull(json_contains_path(doc, 'one', '$.arr'), 0)"
        ),
        1
    );
    assert_ids(&mut adapter, &[("1 = 1", &[4, 5, 6, 7])]);
}

/// What has not been measured, or reads differently, is refused rather than
/// answered some other way.
#[test]
fn what_mysql_reads_some_other_way_is_refused() {
    let (_directory, mut adapter) = adapter();
    for condition in [
        // MySQL answers these, but no measured rule here matches them.
        "doc->>'$.tags[*]' = 'x'",
        "doc->>'$ . lang' = 'en'",
        "doc->>'$.lang' = 'en' COLLATE utf8mb4_0900_ai_ci",
        "doc->>'$.lang' LIKE 'e%'",
        "doc->>'$.lang' IN ('en', 'EN')",
        // MySQL reads this `TRUE` as the JSON `true`, not as the number 1.
        "doc->'$.a' = TRUE",
        "json_length(doc) = '6'",
        // Errors in MySQL: 3143, 3141 and 3149.
        "doc->>'$.1a' = 'x'",
        "json_contains(doc, 'x', '$.tags')",
        "json_contains(doc, '1', '$.tags[*]')",
        // A text column is read as a document first, which is not measured.
        "note->>'$.a' = 'x'",
        "json_contains(note, '1')",
    ] {
        let sql = format!("SELECT id FROM items WHERE {condition}");
        assert!(adapter.execute_query(&sql).is_err(), "{sql}");
    }
    assert!(adapter
        .execute_query("UPDATE items SET seen = 1 WHERE note->>'$.a' = 'x'")
        .is_err());
}

fn execute(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    statement_id: u32,
    payload: &[u8],
) -> Result<Vec<i64>, FrontendErrorKind> {
    let PreparedStatementExecutionResult::ResultSet(result) =
        adapter.execute_stmt_execute(statement_id, payload)?
    else {
        panic!("a SELECT must answer rows");
    };
    Ok(result
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [BinaryResultValue::Integer(id)] => *id,
            other => panic!("unexpected row {other:?}"),
        })
        .collect())
}

/// The null bitmap, the new-parameters flag and one VAR_STRING parameter.
fn a_word(word: &str) -> Vec<u8> {
    let mut payload = vec![
        0,
        1,
        MYSQL_TYPE_VAR_STRING,
        0,
        u8::try_from(word.len()).unwrap(),
    ];
    payload.extend_from_slice(word.as_bytes());
    payload
}

/// The null bitmap, the new-parameters flag and one LONGLONG parameter.
fn a_number(number: i64) -> Vec<u8> {
    let mut payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
    payload.extend_from_slice(&number.to_le_bytes());
    payload
}

/// A bound value is compared by what it binds as — a word as a word and a
/// number as a number — which is what MySQL does until the statement has once
/// been bound a number: measured, it then reads every later word as a number
/// too, so a word is refused from then on.
#[test]
fn a_bound_value_is_compared_by_what_it_binds_as() {
    let (_directory, mut adapter) = adapter();
    let statement = adapter
        .execute_stmt_prepare("SELECT id FROM items WHERE doc->>'$.a' = ? ORDER BY id")
        .unwrap();
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &a_word("1.0")),
        Ok(vec![])
    );
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &a_word("1")),
        Ok(vec![1, 2])
    );
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &a_number(1)),
        Ok(vec![1, 2])
    );
    assert!(execute(&mut adapter, statement.statement_id, &a_word("1.0")).is_err());

    let statement = adapter
        .execute_stmt_prepare(
            r#"select id from items where json_unquote(json_extract(`doc`, '$."lang"')) = ? order by id"#,
        )
        .unwrap();
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &a_word("en")),
        Ok(vec![1, 3])
    );
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &a_word("EN")),
        Ok(vec![2])
    );

    // `JSON_CONTAINS` looks for a document bound as text; a number finds
    // nothing in MySQL and then makes it refuse every word, so both are
    // refused here.
    let statement = adapter
        .execute_stmt_prepare(
            r#"select id from items where json_contains(`doc`, ?, '$."tags"') order by id"#,
        )
        .unwrap();
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &a_word(r#""x""#)),
        Ok(vec![1])
    );
    assert!(execute(&mut adapter, statement.statement_id, &a_word("x")).is_err());
    assert!(execute(&mut adapter, statement.statement_id, &a_number(1)).is_err());
    assert!(execute(&mut adapter, statement.statement_id, &a_word(r#""x""#)).is_err());

    // `JSON_LENGTH` counts, and MySQL reads a bound word as a number there.
    let statement = adapter
        .execute_stmt_prepare(
            "SELECT id FROM items WHERE json_length(doc, '$.tags') = ? ORDER BY id",
        )
        .unwrap();
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &a_number(2)),
        Ok(vec![1])
    );
    assert!(execute(&mut adapter, statement.statement_id, &a_word("2")).is_err());

    // A prepared write has no step holding what binds, so it is refused.
    assert!(adapter
        .execute_stmt_prepare("UPDATE items SET seen = 1 WHERE doc->>'$.lang' = ?")
        .is_err());
}
