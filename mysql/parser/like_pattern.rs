use super::SessionSqlMode;

/// One `SHOW ... LIKE` pattern, split into the units MySQL matches with.
///
/// `SHOW ... LIKE` does not reuse the `LIKE` operator's collation rules. It
/// always matches case-insensitively and it never trims trailing spaces.
///
/// `NO_BACKSLASH_ESCAPES` changes this layer, not only the string literal.
/// Measured on MySQL 8.4.11: `SHOW VARIABLES LIKE 'sql\_mode'` returns
/// `sql_mode` under the default mode and returns nothing once
/// `NO_BACKSLASH_ESCAPES` is set, while `'sql_mod_'` keeps matching under both.
/// The literal is identical in both modes, because MySQL keeps the backslash in
/// `\_` and `\%` either way, so only the matcher can explain the difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlLikePattern {
    units: Vec<PatternUnit>,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternUnit {
    /// `%`: any run of characters, including none.
    AnyRun,
    /// `_`: exactly one character.
    OneCharacter,
    Literal(char),
}

impl MySqlLikePattern {
    /// Splits one already-unquoted pattern into its matching units.
    pub fn new(pattern: &str, mode: SessionSqlMode) -> Self {
        let mut units = Vec::new();
        let mut characters = pattern.chars();
        while let Some(character) = characters.next() {
            units.push(match character {
                '%' => PatternUnit::AnyRun,
                '_' => PatternUnit::OneCharacter,
                '\\' if !mode.no_backslash_escapes => {
                    PatternUnit::Literal(characters.next().unwrap_or('\\'))
                }
                other => PatternUnit::Literal(other),
            });
        }
        Self {
            units,
            text: pattern.to_owned(),
        }
    }

    /// Splits one pattern whose escape character was named rather than assumed.
    ///
    /// `JSON_SEARCH` takes the escape as an argument of its own, and no escape
    /// at all when that argument is empty.
    pub fn with_escape(pattern: &str, escape: Option<char>) -> Self {
        let mut units = Vec::new();
        let mut characters = pattern.chars();
        while let Some(character) = characters.next() {
            units.push(match character {
                '%' => PatternUnit::AnyRun,
                '_' => PatternUnit::OneCharacter,
                other if Some(other) == escape => {
                    PatternUnit::Literal(characters.next().unwrap_or(other))
                }
                other => PatternUnit::Literal(other),
            });
        }
        Self {
            units,
            text: pattern.to_owned(),
        }
    }

    /// Returns the pattern as it was written.
    ///
    /// `SHOW TABLES LIKE 'alpha%'` names its column `Tables_in_probe (alpha%)`,
    /// so the text has to survive the split into units.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Reports whether the pattern covers the whole of `subject`, matching
    /// letters whatever their case.
    pub fn matches(&self, subject: &str) -> bool {
        self.covers(subject, false)
    }

    /// Reports whether the pattern covers the whole of `subject`, telling one
    /// case of a letter from the other.
    ///
    /// Measured on MySQL 8.4.11: `SHOW TABLES LIKE 'ALPHA%'` answers nothing
    /// where `SHOW COLUMNS FROM alpha LIKE 'ID'` answers the `id` column, so a
    /// table name is the one `SHOW ... LIKE` subject that keeps its case.
    pub fn matches_keeping_case(&self, subject: &str) -> bool {
        self.covers(subject, true)
    }

    fn covers(&self, subject: &str, keep_case: bool) -> bool {
        let subject = subject.chars().collect::<Vec<_>>();
        let mut unit = 0;
        let mut position = 0;
        let mut last_run = None;
        let mut run_position = 0;
        while position < subject.len() {
            match self.units.get(unit) {
                Some(PatternUnit::AnyRun) => {
                    last_run = Some(unit);
                    run_position = position;
                    unit += 1;
                }
                Some(PatternUnit::OneCharacter) => {
                    unit += 1;
                    position += 1;
                }
                Some(PatternUnit::Literal(expected))
                    if if keep_case {
                        *expected == subject[position]
                    } else {
                        expected.eq_ignore_ascii_case(&subject[position])
                    } =>
                {
                    unit += 1;
                    position += 1;
                }
                Some(_) | None => {
                    // Give the most recent `%` one more character and retry the
                    // units after it.
                    let Some(run) = last_run else {
                        return false;
                    };
                    run_position += 1;
                    unit = run + 1;
                    position = run_position;
                }
            }
        }
        self.units[unit..]
            .iter()
            .all(|unit| *unit == PatternUnit::AnyRun)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, subject: &str) -> bool {
        MySqlLikePattern::new(pattern, SessionSqlMode::default()).matches(subject)
    }

    fn matches_without_backslash_escapes(pattern: &str, subject: &str) -> bool {
        let mode = SessionSqlMode {
            no_backslash_escapes: true,
            ..SessionSqlMode::default()
        };
        MySqlLikePattern::new(pattern, mode).matches(subject)
    }

    #[test]
    fn wildcards_match_what_mysql_8_4_matched() {
        assert!(matches("sql_mod_", "sql_mode"));
        assert!(matches("sql%mode", "sql_mode"));
        assert!(matches(r"sql\_mode", "sql_mode"));
        assert!(matches("SQL\\_MODE", "sql_mode"));
        assert!(matches("SQL_MODE", "sql_mode"));
        assert!(!matches(r"sql\_mod\_", "sql_mode"));
        assert!(!matches(r"sql\%mode", "sql_mode"));
        assert!(!matches("sqlXmode", "sql_mode"));
        assert!(!matches("sql_mode ", "sql_mode"));
        assert!(!matches("", "sql_mode"));
    }

    #[test]
    fn no_backslash_escapes_makes_the_backslash_an_ordinary_character() {
        assert!(!matches_without_backslash_escapes(r"sql\_mode", "sql_mode"));
        assert!(matches_without_backslash_escapes("sql_mod_", "sql_mode"));
        assert!(!matches_without_backslash_escapes(
            r"sql\_mod\_",
            "sql_mode"
        ));
        assert!(matches_without_backslash_escapes(
            r"sql\_mode",
            r"sql\_mode"
        ));
    }

    #[test]
    fn a_run_stretches_only_as_far_as_the_rest_of_the_pattern_allows() {
        assert!(matches("%", "gtid_mode"));
        assert!(matches("%mode", "gtid_mode"));
        assert!(matches("gtid%", "gtid_mode"));
        assert!(matches("%id%mo%", "gtid_mode"));
        assert!(matches("g%%%e", "gtid_mode"));
        assert!(!matches("%mode%x", "gtid_mode"));
        assert!(!matches("gtid%modex", "gtid_mode"));
        assert!(matches("", ""));
        assert!(matches("%", ""));
    }

    #[test]
    fn a_table_name_keeps_its_case_and_the_pattern_keeps_its_text() {
        let pattern = MySqlLikePattern::new("Alpha%", SessionSqlMode::default());
        assert_eq!(pattern.text(), "Alpha%");
        assert!(pattern.matches("alpha_two"));
        assert!(!pattern.matches_keeping_case("alpha_two"));
        assert!(pattern.matches_keeping_case("Alpha_two"));
        assert!(
            MySqlLikePattern::new(r"alpha\_two", SessionSqlMode::default())
                .matches_keeping_case("alpha_two")
        );
    }

    #[test]
    fn a_trailing_backslash_matches_a_backslash() {
        assert!(matches("a\\", "a\\"));
        assert!(!matches("a\\", "ab"));
    }
}
