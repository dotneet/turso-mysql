//! MySQL's `utf8mb4_general_ci`: each character has one weight, and two
//! strings compare by their weights with the shorter one padded with spaces.
//!
//! The weights are in `mysql_general_ci_weights.rs`, which
//! `generate_mysql_general_ci.py` writes from MySQL 8.4.11's own
//! `WEIGHT_STRING()` of every code point. No MySQL source code or table is
//! copied.

use std::{cmp::Ordering, hash::Hasher};

use super::collate::{shared_prefix_len, sort_key_on_stack, STACK_SORT_KEY_LEN};
use super::mysql_general_ci_weights::{PAGES, PAGE_OF};

/// The weight of a space. A shorter string compares as if padded with it, so
/// `'a' = 'a '`, while a tab and a NUL weigh less and `'a' > 'a\t'`.
const SPACE: u16 = 0x0020;
/// Measured on MySQL 8.4.11: every character past the Basic Multilingual Plane
/// weighs this, so all of them are equal to each other and to U+FFFD.
const OUTSIDE_THE_BMP: u16 = 0xfffd;

/// Compare the weights, the shorter string padded with spaces.
pub fn compare(lhs: &str, rhs: &str) -> Ordering {
    let shared = shared_prefix_len(lhs, rhs);
    let mut lhs = lhs[shared..].chars().map(weight);
    let mut rhs = rhs[shared..].chars().map(weight);
    loop {
        match (lhs.next(), rhs.next()) {
            (Some(left), Some(right)) if left != right => return left.cmp(&right),
            (Some(_), Some(_)) => {}
            (None, None) => return Ordering::Equal,
            (Some(left), None) => {
                return std::iter::once(left)
                    .chain(lhs)
                    .map(|weight| weight.cmp(&SPACE))
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal)
            }
            (None, Some(right)) => {
                return std::iter::once(right)
                    .chain(rhs)
                    .map(|weight| SPACE.cmp(&weight))
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal)
            }
        }
    }
}

/// A byte key that is equal for exactly the strings [`compare`] finds equal,
/// for hash joins and grouping.
pub(crate) fn sort_key(text: &str) -> Vec<u8> {
    let mut weights = text.chars().map(weight).collect::<Vec<_>>();
    while weights.last() == Some(&SPACE) {
        weights.pop();
    }
    weights
        .into_iter()
        .flat_map(|weight| weight.to_be_bytes())
        .collect()
}

pub(crate) fn leading_weights(text: &str) -> ([u16; 3], bool) {
    let mut weights = text.chars().map(weight);
    let mut leading = [SPACE; 3];
    for slot in &mut leading {
        let Some(weight) = weights.next() else {
            return (leading, true);
        };
        *slot = weight;
    }
    (leading, weights.next().is_none())
}

pub(crate) fn write_sort_key(text: &str, hasher: &mut impl Hasher) {
    let mut key = [0; STACK_SORT_KEY_LEN];
    let len = if text.is_ascii() {
        sort_key_on_stack(
            &mut key,
            text.bytes().map(|byte| ASCII_WEIGHTS[usize::from(byte)]),
        )
    } else {
        sort_key_on_stack(&mut key, text.chars().map(weight))
    };
    match len {
        Some(mut len) => {
            while len >= 2 && key[len - 2..len] == SPACE.to_be_bytes() {
                len -= 2;
            }
            hasher.write(&key[..len]);
        }
        None => hasher.write(&sort_key(text)),
    }
}

/// Match MySQL LIKE one character at a time, without padding, a character
/// matching every character of the same weight. Measured on MySQL 8.4.11:
/// `'ß' LIKE 's'`, `'é' LIKE 'E'` and two different emoji match, while
/// `'ß' LIKE 'ss'` and `'a ' LIKE 'a'` do not.
pub fn like(text: &str, pattern: &str, escape: Option<char>) -> crate::Result<bool> {
    super::mysql_like::like(text, pattern, escape, |lhs, rhs| weight(lhs) == weight(rhs))
}

#[inline(always)]
fn weight(character: char) -> u16 {
    let code_point = u32::from(character);
    if code_point < 128 {
        return ASCII_WEIGHTS[code_point as usize];
    }
    let Ok(code_point) = u16::try_from(code_point) else {
        return OUTSIDE_THE_BMP;
    };
    match PAGE_OF[usize::from(code_point >> 8)] {
        0 => code_point,
        page => PAGES[usize::from(page) - 1][usize::from(code_point & 0xff)],
    }
}

const ASCII_WEIGHTS: [u16; 128] = ascii_weights();

const fn ascii_weights() -> [u16; 128] {
    assert!(PAGE_OF[0] != 0, "ASCII letters weigh their capitals");
    let page = &PAGES[PAGE_OF[0] as usize - 1];
    let mut weights = [0; 128];
    let mut code_point = 0;
    while code_point < 128 {
        weights[code_point] = page[code_point];
        code_point += 1;
    }
    weights
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each weight measured on MySQL 8.4.11 with `HEX(WEIGHT_STRING(c COLLATE
    /// utf8mb4_general_ci))`.
    #[test]
    fn characters_have_the_weights_mysql_gives_them() {
        for (character, expected) in [
            ('\0', 0x0000),
            ('\t', 0x0009),
            (' ', 0x0020),
            ('a', 0x0041),
            ('A', 0x0041),
            ('_', 0x005f),
            ('ß', 0x0053),
            ('é', 0x0045),
            ('İ', 0x0049),
            ('ı', 0x0049),
            ('ſ', 0x0053),
            ('ǅ', 0x01c4),
            ('ǆ', 0x01c4),
            ('ς', 0x03a3),
            ('\u{fffd}', 0xfffd),
            ('😀', 0xfffd),
            ('\u{10000}', 0xfffd),
            ('\u{10ffff}', 0xfffd),
        ] {
            assert_eq!(weight(character), expected, "{character:?}");
        }
    }

    #[test]
    fn table_has_the_measured_shape() {
        assert_eq!(PAGES.len(), 11);
        let differing = (0..=0xffffu32)
            .filter_map(char::from_u32)
            .filter(|&character| u32::from(weight(character)) != u32::from(character))
            .count();
        assert_eq!(differing, 1108);
    }

    /// Each pair measured on MySQL 8.4.11 with `utf8mb4_general_ci`.
    #[test]
    fn strings_compare_padded_with_spaces_and_ignoring_case_and_accents() {
        for (left, right, expected) in [
            ("a", "A", Ordering::Equal),
            ("a", "a ", Ordering::Equal),
            ("", "  ", Ordering::Equal),
            ("a\t", "a", Ordering::Less),
            ("a\0", "a", Ordering::Less),
            ("a\0", "a\t", Ordering::Less),
            ("ß", "s", Ordering::Equal),
            ("ß", "ss", Ordering::Less),
            ("é", "E", Ordering::Equal),
            ("😀", "😃", Ordering::Equal),
            ("😀", "\u{fffd}", Ordering::Equal),
            ("ı", "i", Ordering::Equal),
            ("İ", "i", Ordering::Equal),
            ("ǅ", "ǆ", Ordering::Equal),
            ("æ", "ae", Ordering::Greater),
            ("_", "a", Ordering::Greater),
            ("`", "a", Ordering::Greater),
            ("ß", "t", Ordering::Less),
            ("straße", "stras", Ordering::Greater),
            ("straße", "strasse", Ordering::Less),
        ] {
            assert_eq!(compare(left, right), expected, "{left:?} / {right:?}");
            assert_eq!(
                compare(right, left),
                expected.reverse(),
                "{right:?} / {left:?}"
            );
            assert_eq!(
                sort_key(left) == sort_key(right),
                expected.is_eq(),
                "{left:?} / {right:?}"
            );
        }
    }

    #[test]
    fn random_texts_compare_and_hash_by_their_weights() {
        for (left, right) in super::super::collate::test_text::similar_pairs(400, 200_000) {
            assert_eq!(
                compare(&left, &right),
                compare_padded_with_spaces(&left, &right),
                "{left:?} / {right:?}"
            );
            assert_eq!(
                sort_key(&left) == sort_key(&right),
                compare(&left, &right).is_eq(),
                "{left:?} / {right:?}"
            );
        }
    }

    fn compare_padded_with_spaces(lhs: &str, rhs: &str) -> Ordering {
        let mut lhs = lhs.chars().map(weight).collect::<Vec<_>>();
        let mut rhs = rhs.chars().map(weight).collect::<Vec<_>>();
        let len = lhs.len().max(rhs.len());
        lhs.resize(len, SPACE);
        rhs.resize(len, SPACE);
        lhs.cmp(&rhs)
    }

    /// Each match measured on MySQL 8.4.11 with `utf8mb4_general_ci`.
    #[test]
    fn mysql_like_matches_characters_of_the_same_weight_without_padding() {
        for (text, pattern, expected) in [
            ("ß", "s", true),
            ("ß", "ss", false),
            ("😀", "😃", true),
            ("😀", "_", true),
            ("😀", "\u{fffd}", true),
            ("a ", "a", false),
            ("a", "a ", false),
            ("A", "a", true),
            ("é", "E", true),
            ("Straße", "STRAS%", true),
            ("ǅ", "Ǆ", true),
            ("a%", "a!%", true),
        ] {
            let escape = pattern.contains('!').then_some('!');
            assert_eq!(
                like(text, pattern, escape).unwrap(),
                expected,
                "{text:?} LIKE {pattern:?}"
            );
        }
    }
}
