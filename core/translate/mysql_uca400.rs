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
    let shared = super::collate::shared_prefix_len(lhs, rhs);
    let mut lhs = PrimaryWeights::new(&lhs[shared..]);
    let mut rhs = PrimaryWeights::new(&rhs[shared..]);
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
    super::mysql_like::like(text, pattern, escape, same_character)
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
            if let Some(&weight) = ASCII_WEIGHTS.get(codepoint as usize) {
                if weight != 0 {
                    return Some(weight);
                }
                continue;
            }
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

const ASCII_WEIGHTS: [u16; 128] = ascii_weights();

const fn ascii_weights() -> [u16; 128] {
    let mut weights = [0; 128];
    let mut codepoint = 0;
    while codepoint < 128 {
        let Some((offset, count)) = explicit_weights(codepoint as u16) else {
            panic!("every ASCII character has an entry in the table");
        };
        assert!(count <= 1, "no ASCII character has more than one weight");
        if count == 1 {
            weights[codepoint] = u16::from_be_bytes([DATA[offset], DATA[offset + 1]]);
            assert!(weights[codepoint] != 0, "the table holds no zero weight");
        }
        codepoint += 1;
    }
    weights
}

const fn explicit_weights(codepoint: u16) -> Option<(usize, usize)> {
    let count = read_u32(8) as usize;
    let weights_start = HEADER_LEN + count * RECORD_LEN;
    let mut lo = 0;
    let mut hi = count;
    while lo < hi {
        let middle = lo + (hi - lo) / 2;
        let record = HEADER_LEN + middle * RECORD_LEN;
        let record_codepoint = read_u16(record);
        if record_codepoint < codepoint {
            lo = middle + 1;
        } else if record_codepoint > codepoint {
            hi = middle;
        } else {
            let offset = read_u16(record + 2) as usize;
            let len = DATA[record + 4] as usize;
            return Some((weights_start + offset * 2, len));
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

const fn read_u16(offset: usize) -> u16 {
    u16::from_le_bytes([DATA[offset], DATA[offset + 1]])
}

const fn read_u32(offset: usize) -> u32 {
    u32::from_le_bytes([
        DATA[offset],
        DATA[offset + 1],
        DATA[offset + 2],
        DATA[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_character_has_the_weights_of_the_table() {
        for codepoint in 0..=0x10ffff {
            let Some(character) = char::from_u32(codepoint) else {
                continue;
            };
            let mut bytes = [0; 4];
            let text = character.encode_utf8(&mut bytes);
            assert_eq!(
                PrimaryWeights::new(text).collect::<Vec<_>>(),
                weights_from_the_table(text),
                "U+{codepoint:04X}"
            );
        }
    }

    #[test]
    fn every_pair_of_ascii_characters_compares_by_the_weights_of_the_table() {
        let texts: Vec<String> = std::iter::once(String::new())
            .chain((0..128u8).map(|byte| char::from(byte).to_string()))
            .collect();
        for left in &texts {
            for right in &texts {
                assert_eq!(
                    compare(left, right),
                    compare_padded_with_spaces(left, right),
                    "{left:?} / {right:?}"
                );
            }
        }
    }

    #[test]
    fn random_texts_compare_and_hash_by_the_weights_of_the_table() {
        for (left, right) in super::super::collate::test_text::similar_pairs(400, 200_000) {
            assert_eq!(
                compare(&left, &right),
                compare_padded_with_spaces(&left, &right),
                "{left:?} / {right:?}"
            );
            let mut weights = weights_from_the_table(&left);
            while weights.last() == Some(&SPACE) {
                weights.pop();
            }
            assert_eq!(
                sort_key(&left),
                weights
                    .into_iter()
                    .flat_map(u16::to_be_bytes)
                    .collect::<Vec<_>>(),
                "{left:?}"
            );
        }
    }

    fn compare_padded_with_spaces(lhs: &str, rhs: &str) -> Ordering {
        let mut lhs = weights_from_the_table(lhs);
        let mut rhs = weights_from_the_table(rhs);
        let len = lhs.len().max(rhs.len());
        lhs.resize(len, SPACE);
        rhs.resize(len, SPACE);
        lhs.cmp(&rhs)
    }

    fn weights_from_the_table(text: &str) -> Vec<u16> {
        let mut weights = Vec::new();
        for character in text.chars() {
            let Ok(codepoint) = u16::try_from(u32::from(character)) else {
                weights.push(OUTSIDE_THE_BMP);
                continue;
            };
            match explicit_weights(codepoint) {
                Some((offset, count)) => weights.extend(
                    DATA[offset..offset + count * 2]
                        .chunks(2)
                        .map(|pair| u16::from_be_bytes([pair[0], pair[1]])),
                ),
                None => weights.extend(implicit_weights(codepoint)),
            }
        }
        weights
    }

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
