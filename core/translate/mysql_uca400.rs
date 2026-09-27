//! Frozen Unicode 4.0.0 primary weights for MySQL's `utf8mb4_unicode_ci`.
//!
//! The table is generated from Unicode's `allkeys-4.0.0.txt` by
//! `generate_mysql_uca400.py`. Copyright 1991-2004 Unicode, Inc. The Unicode
//! data license is in `licenses/core/unicode-data-license.md`. No MySQL source
//! tables are copied. The source file and its SHA-256 hash are in the
//! generator.
//!
//! Every weight below was checked against MySQL 8.4.11's `WEIGHT_STRING()` for
//! each character of the Basic Multilingual Plane.

use std::{cmp::Ordering, str::Chars};

const DATA: &[u8] = include_bytes!("mysql_uca400_primary.bin");
const HEADER_LEN: usize = 16;
const RECORD_LEN: usize = 5;
/// The weight of a space. A shorter string compares as if padded with it, so
/// `'a' = 'a '`, while a tab weighs less than a space and `'a' > 'a\t'`.
const SPACE: u16 = 0x0209;
/// MySQL gives every character outside the Basic Multilingual Plane this one
/// weight, so all of them compare equal.
const OUTSIDE_THE_BMP: u16 = 0xfffd;

/// Compare the primary collation weights, ignoring accents and case, with the
/// shorter string padded with spaces.
pub fn compare(lhs: &str, rhs: &str) -> Ordering {
    let mut lhs = PrimaryWeights::new(lhs);
    let mut rhs = PrimaryWeights::new(rhs);
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
/// for hash joins and grouping. It does not sort in collation order.
pub(crate) fn sort_key(text: &str) -> Vec<u8> {
    let mut weights = PrimaryWeights::new(text).collect::<Vec<_>>();
    while weights.last() == Some(&SPACE) {
        weights.pop();
    }
    weights
        .into_iter()
        .flat_map(|weight| weight.to_be_bytes())
        .collect()
}

/// Match MySQL LIKE one character at a time, without padding. Measured on
/// MySQL 8.4.11: `'á' LIKE 'A'` and `'ß' LIKE '_'` match, but `'ß' LIKE 'ss'`,
/// `'a ' LIKE 'a'` and two different emoji do not.
pub fn like(text: &str, pattern: &str, escape: Option<char>) -> crate::Result<bool> {
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

/// Characters outside the Basic Multilingual Plane share one weight, but
/// MySQL's LIKE tells them apart by their code point.
fn same_character(lhs: char, rhs: char) -> bool {
    if u32::from(lhs) > 0xffff || u32::from(rhs) > 0xffff {
        return lhs == rhs;
    }
    let mut lhs_bytes = [0; 4];
    let mut rhs_bytes = [0; 4];
    PrimaryWeights::new(lhs.encode_utf8(&mut lhs_bytes))
        .eq(PrimaryWeights::new(rhs.encode_utf8(&mut rhs_bytes)))
}

struct PrimaryWeights<'a> {
    characters: Chars<'a>,
    offset: usize,
    remaining: usize,
    pending: [u16; 2],
    pending_next: usize,
}

impl<'a> PrimaryWeights<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            characters: text.chars(),
            offset: 0,
            remaining: 0,
            pending: [0; 2],
            pending_next: 2,
        }
    }
}

impl Iterator for PrimaryWeights<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.remaining > 0 {
                let bytes = &DATA[self.offset..self.offset + 2];
                self.offset += 2;
                self.remaining -= 1;
                return Some(u16::from_be_bytes(bytes.try_into().unwrap()));
            }
            if self.pending_next < 2 {
                let weight = self.pending[self.pending_next];
                self.pending_next += 1;
                return Some(weight);
            }
            let codepoint = u32::from(self.characters.next()?);
            let Ok(codepoint) = u16::try_from(codepoint) else {
                return Some(OUTSIDE_THE_BMP);
            };
            if let Some((offset, count)) = explicit_weights(codepoint) {
                self.offset = offset;
                self.remaining = count;
            } else {
                self.pending = implicit_weights(codepoint);
                self.pending_next = 0;
            }
        }
    }
}

fn explicit_weights(codepoint: u16) -> Option<(usize, usize)> {
    let count = read_u32(8) as usize;
    let weights_start = HEADER_LEN + count * RECORD_LEN;
    let mut lo = 0;
    let mut hi = count;
    while lo < hi {
        let middle = lo + (hi - lo) / 2;
        let record = HEADER_LEN + middle * RECORD_LEN;
        match read_u16(record).cmp(&codepoint) {
            Ordering::Less => lo = middle + 1,
            Ordering::Greater => hi = middle,
            Ordering::Equal => {
                let offset = read_u16(record + 2) as usize;
                let len = DATA[record + 4] as usize;
                return Some((weights_start + offset * 2, len));
            }
        }
    }
    None
}

/// The two weights of a character `allkeys-4.0.0.txt` has no entry for,
/// Hangul syllables included: MySQL does not decompose them.
fn implicit_weights(codepoint: u16) -> [u16; 2] {
    let base = match codepoint {
        0x3400..=0x4db5 => 0xfb80,
        0x4e00..=0x9fa5 => 0xfb40,
        _ => 0xfbc0,
    };
    [base + (codepoint >> 15), (codepoint & 0x7fff) | 0x8000]
}

fn read_u16(offset: usize) -> u16 {
    u16::from_le_bytes(DATA[offset..offset + 2].try_into().unwrap())
}

fn read_u32(offset: usize) -> u32 {
    u32::from_le_bytes(DATA[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_table_has_expected_shape() {
        assert_eq!(&DATA[..8], b"UCA400P1");
        assert_eq!(read_u32(8), 12_072);
        assert_eq!(read_u32(12), 14_119);
        assert_eq!(DATA.len(), 88_614);
    }

    #[test]
    fn oracle_primary_weights_for_explicit_and_implicit_characters() {
        for (input, expected) in [
            ("a", "0e33"),
            ("A", "0e33"),
            ("é", "0e8b"),
            ("ß", "0fea0fea"),
            ("æ", "0e38"),
            ("\t", "0201"),
            (" ", "0209"),
            ("\u{1}", ""),
            ("一", "fb40ce00"),
            ("㐀", "fb80b400"),
            ("가", "fbc1ac00"),
            ("\u{fdfa}", "fbc1fdfa"),
            ("\u{fdfd}", "034f"),
            ("😀", "fffd"),
        ] {
            let hex = PrimaryWeights::new(input)
                .map(|weight| format!("{weight:04x}"))
                .collect::<String>();
            assert_eq!(hex, expected, "{input:?}");
        }
    }

    /// Each pair measured on MySQL 8.4.11 with `utf8mb4_unicode_ci`.
    #[test]
    fn strings_compare_padded_with_spaces_and_ignoring_case_and_accents() {
        for (left, right, expected) in [
            ("a", "A", Ordering::Equal),
            ("a", "á", Ordering::Equal),
            ("a", "a ", Ordering::Equal),
            ("a", "a\u{1}", Ordering::Equal),
            ("ß", "ss", Ordering::Equal),
            ("😀", "😃", Ordering::Equal),
            ("a", "a\t", Ordering::Greater),
            ("a\t", "a", Ordering::Less),
            ("ae", "æ", Ordering::Less),
            ("a", "b", Ordering::Less),
            ("", " ", Ordering::Equal),
        ] {
            assert_eq!(compare(left, right), expected, "{left:?} / {right:?}");
            assert_eq!(
                sort_key(left) == sort_key(right),
                expected.is_eq(),
                "{left:?} / {right:?}"
            );
        }
    }

    /// Each match measured on MySQL 8.4.11 with `utf8mb4_unicode_ci`.
    #[test]
    fn mysql_like_matches_one_character_at_a_time_without_padding() {
        for (text, pattern, expected) in [
            ("á", "A", true),
            ("Straße", "stra%", true),
            ("ß", "_", true),
            ("ß", "ss", false),
            ("a ", "a", false),
            ("a\u{1}", "a", false),
            ("😀", "😃", false),
            ("😀", "😀", true),
            ("😀", "_", true),
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
