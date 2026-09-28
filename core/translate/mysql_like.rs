//! MySQL's `LIKE`, matched one character at a time without padding. A
//! collation decides only when two characters are the same one.

/// Matches under a binary collation such as `utf8mb4_bin`, where a character
/// is the same only as itself. Measured on MySQL 8.4.11 over the text
/// `JSON_UNQUOTE` answers: `'Osaka' LIKE 'osa%'` does not match, and
/// `'Tokyo' LIKE 'T_kyo'` does.
pub fn binary_like(text: &str, pattern: &str, escape: Option<char>) -> crate::Result<bool> {
    like(text, pattern, escape, |lhs, rhs| lhs == rhs)
}

pub(crate) fn like(
    text: &str,
    pattern: &str,
    escape: Option<char>,
    same_character: impl Fn(char, char) -> bool,
) -> crate::Result<bool> {
    const MAX_PATTERN_BYTES: usize = 50_000;
    const MAX_MATCH_STEPS: usize = 10_000_000;
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(crate::LimboError::Constraint(
            "LIKE pattern too complex".to_owned(),
        ));
    }

    let mut units = Vec::new();
    let mut pattern_chars = pattern.chars();
    while let Some(character) = pattern_chars.next() {
        let unit = if escape == Some(character) {
            LikeUnit::Literal(pattern_chars.next().unwrap_or(character))
        } else {
            match character {
                '%' => LikeUnit::AnyMany,
                '_' => LikeUnit::AnyOne,
                _ => LikeUnit::Literal(character),
            }
        };
        if unit != LikeUnit::AnyMany || units.last() != Some(&LikeUnit::AnyMany) {
            units.push(unit);
        }
    }

    let (mut text_index, mut pattern_index) = (0, 0);
    let mut last_many = None;
    let mut steps = 0;
    while text_index < text.len() {
        steps += 1;
        if steps > MAX_MATCH_STEPS {
            return Err(crate::LimboError::Constraint(
                "LIKE match too complex".to_owned(),
            ));
        }
        let character = text[text_index..].chars().next().unwrap();
        match units.get(pattern_index) {
            Some(LikeUnit::AnyMany) => {
                pattern_index += 1;
                last_many = Some((pattern_index, text_index));
            }
            Some(LikeUnit::AnyOne) => {
                text_index += character.len_utf8();
                pattern_index += 1;
            }
            Some(LikeUnit::Literal(pattern_character))
                if same_character(character, *pattern_character) =>
            {
                text_index += character.len_utf8();
                pattern_index += 1;
            }
            _ => {
                let Some((after_many, consumed)) = last_many else {
                    return Ok(false);
                };
                text_index = consumed + text[consumed..].chars().next().unwrap().len_utf8();
                pattern_index = after_many;
                last_many = Some((after_many, text_index));
            }
        }
    }
    Ok(units[pattern_index..]
        .iter()
        .all(|unit| *unit == LikeUnit::AnyMany))
}

#[derive(PartialEq, Eq)]
enum LikeUnit {
    Literal(char),
    AnyOne,
    AnyMany,
}

#[cfg(test)]
mod tests {
    use super::binary_like;

    #[test]
    fn a_binary_like_tells_case_apart_and_matches_one_character_per_underscore() {
        assert!(binary_like("Osaka", "Osa%", None).unwrap());
        assert!(!binary_like("Osaka", "osa%", None).unwrap());
        assert!(binary_like("Tokyo", "T_kyo", None).unwrap());
        assert!(binary_like("Tökyo", "T_kyo", None).unwrap());
        assert!(!binary_like("Tokyo", "T\\_kyo", Some('\\')).unwrap());
        assert!(binary_like("T_kyo", "T\\_kyo", Some('\\')).unwrap());
        assert!(!binary_like("a ", "a", None).unwrap());
        assert!(binary_like("Tokyo", "%o", None).unwrap());
    }
}
