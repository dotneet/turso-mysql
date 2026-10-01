//! Frozen Unicode 9 primary weights for MySQL's `utf8mb4_0900_ai_ci`.
//!
//! The table is generated from Unicode 9.0.0 `allkeys.txt` and `PropList.txt`
//! by `generate_mysql_uca9.py`. Copyright 2016 Unicode, Inc. The Unicode data
//! license is in `licenses/core/unicode-data-license.md`. No MySQL source tables are copied.
//! The source files and their SHA-256 hashes are in the generator.

use std::{cmp::Ordering, hash::Hasher, str::Chars};

use super::collate::{sort_key_on_stack, STACK_SORT_KEY_LEN};

const DATA: &[u8] = include_bytes!("mysql_uca9_primary.bin");
const HEADER_LEN: usize = 20;
const RECORD_LEN: usize = 9;

/// Compare the primary collation weights, ignoring accents and case.
pub fn compare(lhs: &str, rhs: &str) -> Ordering {
    let shared = super::collate::shared_prefix_len(lhs, rhs);
    PrimaryWeights::new(&lhs[shared..]).cmp(PrimaryWeights::new(&rhs[shared..]))
}

/// The same primary weights as a byte key for hash joins and grouping.
pub(crate) fn sort_key(text: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(text.len() * 2);
    for weight in PrimaryWeights::new(text) {
        key.extend_from_slice(&weight.to_be_bytes());
    }
    key
}

pub(crate) fn write_sort_key(text: &str, hasher: &mut impl Hasher) {
    let mut key = [0; STACK_SORT_KEY_LEN];
    match sort_key_on_stack(&mut key, PrimaryWeights::new(text)) {
        Some(len) => hasher.write(&key[..len]),
        None => hasher.write(&sort_key(text)),
    }
}

/// Match MySQL LIKE one Unicode scalar at a time. Collation expansions do not
/// change how many characters `_` and literal pattern characters consume.
pub fn like(text: &str, pattern: &str, escape: Option<char>) -> crate::Result<bool> {
    super::mysql_like::like(text, pattern, escape, same_primary_character)
}

fn same_primary_character(lhs: char, rhs: char) -> bool {
    let mut lhs_bytes = [0; 4];
    let mut rhs_bytes = [0; 4];
    compare(
        lhs.encode_utf8(&mut lhs_bytes),
        rhs.encode_utf8(&mut rhs_bytes),
    ) == Ordering::Equal
}

struct PrimaryWeights<'a> {
    characters: Chars<'a>,
    offset: usize,
    remaining: usize,
    implicit: [u16; 2],
    implicit_next: usize,
}

impl<'a> PrimaryWeights<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            characters: text.chars(),
            offset: 0,
            remaining: 0,
            implicit: [0; 2],
            implicit_next: 2,
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
            if self.implicit_next < 2 {
                let weight = self.implicit[self.implicit_next];
                self.implicit_next += 1;
                return Some(weight);
            }
            let codepoint = self.characters.next()? as u32;
            if let Some(&weight) = ASCII_WEIGHTS.get(codepoint as usize) {
                if weight != 0 {
                    return Some(weight);
                }
                continue;
            }
            if let Some((offset, count)) = explicit_weights(codepoint) {
                self.offset = offset;
                self.remaining = count;
            } else {
                self.implicit = implicit_weights(codepoint);
                self.implicit_next = 0;
            }
        }
    }
}

const ASCII_WEIGHTS: [u16; 128] = ascii_weights();

const fn ascii_weights() -> [u16; 128] {
    let mut weights = [0; 128];
    let mut codepoint = 0;
    while codepoint < 128 {
        let Some((offset, count)) = explicit_weights(codepoint as u32) else {
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

const fn explicit_weights(codepoint: u32) -> Option<(usize, usize)> {
    let count = read_u32(8) as usize;
    let weights_start = HEADER_LEN + count * RECORD_LEN;
    let mut lo = 0;
    let mut hi = count;
    while lo < hi {
        let middle = lo + (hi - lo) / 2;
        let record = HEADER_LEN + middle * RECORD_LEN;
        let record_codepoint = read_u32(record);
        if record_codepoint < codepoint {
            lo = middle + 1;
        } else if record_codepoint > codepoint {
            hi = middle;
        } else {
            let offset = read_u32(record + 4) as usize;
            let len = DATA[record + 8] as usize;
            return Some((weights_start + offset * 2, len));
        }
    }
    None
}

fn implicit_weights(codepoint: u32) -> [u16; 2] {
    let (base, remainder) = if (0x17000..=0x18aff).contains(&codepoint) {
        // Tangut uses the dedicated base named in UCA 9 allkeys.txt.
        (0xfb00 + ((codepoint - 0x17000) >> 15), codepoint - 0x17000)
    } else if is_unified_ideograph(codepoint) {
        if (0x4e00..=0x9fff).contains(&codepoint) || (0xf900..=0xfaff).contains(&codepoint) {
            (0xfb40 + (codepoint >> 15), codepoint)
        } else {
            (0xfb80 + (codepoint >> 15), codepoint)
        }
    } else {
        (0xfbc0 + (codepoint >> 15), codepoint)
    };
    [base as u16, ((remainder & 0x7fff) | 0x8000) as u16]
}

fn is_unified_ideograph(codepoint: u32) -> bool {
    let record_count = read_u32(8) as usize;
    let weight_count = read_u32(12) as usize;
    let range_count = read_u32(16) as usize;
    let ranges_start = HEADER_LEN + record_count * RECORD_LEN + weight_count * 2;
    let mut lo = 0;
    let mut hi = range_count;
    while lo < hi {
        let middle = lo + (hi - lo) / 2;
        let range = ranges_start + middle * 8;
        let start = read_u32(range);
        let end = read_u32(range + 4);
        if codepoint < start {
            hi = middle;
        } else if codepoint > end {
            lo = middle + 1;
        } else {
            return true;
        }
    }
    false
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
    fn frozen_table_has_expected_shape() {
        assert_eq!(&DATA[..8], b"UCA9P1\0\0");
        assert_eq!(read_u32(8), 40_981);
        assert_eq!(read_u32(12), 65_049);
        assert_eq!(read_u32(16), 13);
        assert_eq!(DATA.len(), 499_051);
    }

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
                    weights_from_the_table(left).cmp(&weights_from_the_table(right)),
                    "{left:?} / {right:?}"
                );
            }
        }
    }

    #[test]
    fn random_texts_compare_and_hash_by_the_weights_of_the_table() {
        for (left, right) in super::super::collate::test_text::similar_pairs(9, 200_000) {
            assert_eq!(
                compare(&left, &right),
                weights_from_the_table(&left).cmp(&weights_from_the_table(&right)),
                "{left:?} / {right:?}"
            );
            assert_eq!(
                sort_key(&left),
                weights_from_the_table(&left)
                    .into_iter()
                    .flat_map(u16::to_be_bytes)
                    .collect::<Vec<_>>(),
                "{left:?}"
            );
        }
    }

    fn weights_from_the_table(text: &str) -> Vec<u16> {
        let mut weights = Vec::new();
        for character in text.chars() {
            let codepoint = u32::from(character);
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
    fn case_accent_expansion_and_normalization_share_primary_weights() {
        for (left, right) in [
            ("Cafe", "café"),
            ("CAFÉ", "cafe"),
            ("é", "e\u{301}"),
            ("Straße", "strasse"),
            ("İ", "i"),
            ("가", "가"),
        ] {
            assert_eq!(
                compare(left, right),
                Ordering::Equal,
                "{left:?} / {right:?}"
            );
            assert_eq!(sort_key(left), sort_key(right));
        }
    }

    #[test]
    fn no_pad_and_unicode_order_are_preserved() {
        assert_eq!(compare("a", "a "), Ordering::Less);
        assert_eq!(compare("i", "ı"), Ordering::Less);
        assert_eq!(compare("😀", "😃"), Ordering::Less);
        assert_eq!(compare("𐐀", "😀"), Ordering::Greater);
        assert_eq!(compare("\u{4e00}", "\u{20000}"), Ordering::Less);
        assert_eq!(compare("", "a"), Ordering::Less);
    }

    #[test]
    fn oracle_primary_weights_for_explicit_and_implicit_characters() {
        for (input, expected) in [
            ("a", "1c47"),
            ("é", "1caa"),
            ("ß", "1e711e71"),
            ("😀", "15fb"),
            (" ", "0209"),
            ("一", "fb40ce00"),
            ("𠀀", "fb848000"),
            ("𗀀", "fb008000"),
            ("\u{10ffff}", "fbe1ffff"),
            ("ﷺ", "2364239c23c50209230b239c239c23b1"),
        ] {
            let key = sort_key(input);
            let hex = key
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(hex, expected, "{input:?}");
        }
    }

    #[test]
    fn mysql_like_uses_primary_weights_per_character() {
        for (text, pattern, expected) in [
            ("café", "cafe", true),
            ("é", "e", true),
            ("Straße", "stra%", true),
            ("ß", "ss", false),
            ("ß", "_", true),
            ("e\u{301}", "é", false),
            ("e\u{301}", "e_", true),
            ("a\u{200d}", "a", false),
            ("a\u{200d}", "a%", true),
            ("ﬃ", "ffi", false),
            ("😀", "_", true),
            ("a", "a ", false),
            ("a ", "a", false),
            ("a%", "a!%", true),
        ] {
            let escape = pattern.contains('!').then_some('!');
            assert_eq!(
                like(text, pattern, escape).unwrap(),
                expected,
                "{text:?} LIKE {pattern:?}"
            );
        }
        assert!(like("a!", "a!", Some('!')).unwrap());
        assert!(!like("a", "a!", Some('!')).unwrap());
        assert!(like("a%", "a\\%", Some('\\')).unwrap());
        assert!(!like("a", "a\\%", Some('\\')).unwrap());
        assert!(like("abc", "a%%_c", None).unwrap());
        assert!(!like("ab", "a%%_c", None).unwrap());
    }
}
