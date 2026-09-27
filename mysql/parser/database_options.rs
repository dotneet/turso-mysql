//! The collation a `CREATE DATABASE` or `ALTER DATABASE` gives its database.
//!
//! A database's collation is what every table made in it takes when the table
//! names neither a character set nor a collation of its own. This server keeps
//! the two collations a table here can have, and refuses every other one MySQL
//! knows. A name MySQL does not know, or a collation of one character set
//! beside another character set, is the error MySQL answers.

use super::admin_command::{consume_admin_word, AdminToken};
use super::{MySqlTableCollation, ParseError};

/// Reads the options of a `CREATE DATABASE` or `ALTER DATABASE`, and answers
/// the collation they give the database, or `None` when they name neither a
/// character set nor a collation.
///
/// Measured on MySQL 8.4.11: each option may start with `DEFAULT`, take an
/// `=`, be written in any order and more than once, and name its value bare,
/// in backticks or as a string, in any case. A later `COLLATE` replaces an
/// earlier one of the same character set, and a character set named beside a
/// collation of its own leaves that collation in place, whichever comes
/// first. A character set named alone gives the database that character set's
/// default collation, so `ALTER DATABASE d CHARACTER SET utf8mb4` takes a
/// `utf8mb4_unicode_ci` database back to `utf8mb4_0900_ai_ci`. An unknown
/// collation is 1273 and an unknown character set 1115, a collation of
/// another character set than one already named is 1253, and a second
/// character set, or one other than a collation already named belongs to, is
/// 1302 — all of them before anything else about the statement is looked at.
pub(crate) fn consume_database_options(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<Option<MySqlTableCollation>, ParseError> {
    let mut character_set: Option<&'static str> = None;
    let mut collation: Option<String> = None;
    let mut encrypted = false;
    loop {
        let _ = consume_admin_word(tokens, cursor, "DEFAULT");
        let names_a_character_set = if consume_admin_word(tokens, cursor, "CHARACTER") {
            if !consume_admin_word(tokens, cursor, "SET") {
                return Err(ParseError::ExpectedAdminCommand);
            }
            true
        } else {
            consume_admin_word(tokens, cursor, "CHARSET")
        };
        if names_a_character_set {
            let named = known_character_set(option_value(tokens, cursor)?)?;
            if character_set.is_some_and(|already| already != named) {
                return Err(ParseError::ConflictingCharacterSets);
            }
            character_set = Some(named);
        } else if consume_admin_word(tokens, cursor, "COLLATE") {
            let (named, its_character_set) = known_collation(option_value(tokens, cursor)?)?;
            if character_set.is_some_and(|already| already != its_character_set) {
                return Err(ParseError::CollationOfAnotherCharacterSet);
            }
            character_set = Some(its_character_set);
            collation = Some(named);
        } else if consume_admin_word(tokens, cursor, "ENCRYPTION") {
            encrypted |= !option_value(tokens, cursor)?.eq_ignore_ascii_case("N");
        } else {
            break;
        }
    }
    if encrypted {
        return Err(ParseError::Unsupported {
            feature: "database encryption",
        });
    }
    let Some(character_set) = character_set else {
        return Ok(None);
    };
    if character_set != "utf8mb4" {
        return Err(ParseError::Unsupported {
            feature: "database character set other than utf8mb4",
        });
    }
    let collation = collation.as_deref().unwrap_or("utf8mb4_0900_ai_ci");
    MySqlTableCollation::from_name(collation)
        .map(Some)
        .ok_or(ParseError::Unsupported {
            feature: "database collation a table here cannot take",
        })
}

/// Reads an option's value after its optional `=`.
///
/// Measured on MySQL 8.4.11: `CHARACTER SET = DEFAULT` is 1064, so the word
/// is not taken as a name.
fn option_value<'a>(tokens: &'a [AdminToken], cursor: &mut usize) -> Result<&'a str, ParseError> {
    if matches!(tokens.get(*cursor), Some(AdminToken::Equals)) {
        *cursor += 1;
    }
    let value = match tokens.get(*cursor) {
        Some(AdminToken::Word(value)) if !value.eq_ignore_ascii_case("DEFAULT") => value,
        Some(AdminToken::QuotedIdentifier(value) | AdminToken::StringLiteral(value)) => value,
        _ => return Err(ParseError::ExpectedAdminCommand),
    };
    *cursor += 1;
    Ok(value)
}

/// The character set a name means, as MySQL spells it back.
///
/// MySQL reads `utf8` as `utf8mb3`, measured.
fn known_character_set(name: &str) -> Result<&'static str, ParseError> {
    let name = name.to_ascii_lowercase();
    let name = if name == "utf8" { "utf8mb3" } else { &name };
    CHARACTER_SETS
        .iter()
        .find(|known| **known == name)
        .copied()
        .ok_or(ParseError::UnknownCharacterSet)
}

/// The collation a name means, as MySQL spells it back, and its character set.
///
/// Every collation MySQL has is named after its character set, so the
/// character set is the one its name starts with. MySQL reads `utf8_bin` as
/// `utf8mb3_bin`, measured.
fn known_collation(name: &str) -> Result<(String, &'static str), ParseError> {
    let name = name.to_ascii_lowercase();
    let name = match name.strip_prefix("utf8_") {
        Some(rest) => format!("utf8mb3_{rest}"),
        None => name,
    };
    if !COLLATIONS.contains(&name.as_str()) {
        return Err(ParseError::UnknownCollation);
    }
    let character_set = CHARACTER_SETS
        .iter()
        .find(|character_set| {
            name == **character_set
                || name
                    .strip_prefix(**character_set)
                    .is_some_and(|rest| rest.starts_with('_'))
        })
        .copied()
        .expect("every MySQL collation is named after its character set");
    Ok((name, character_set))
}

/// Every character set MySQL 8.4.11 has, from its
/// `information_schema.CHARACTER_SETS`.
const CHARACTER_SETS: [&str; 41] = [
    "armscii8", "ascii", "big5", "binary", "cp1250", "cp1251", "cp1256", "cp1257", "cp850",
    "cp852", "cp866", "cp932", "dec8", "eucjpms", "euckr", "gb18030", "gb2312", "gbk", "geostd8",
    "greek", "hebrew", "hp8", "keybcs2", "koi8r", "koi8u", "latin1", "latin2", "latin5", "latin7",
    "macce", "macroman", "sjis", "swe7", "tis620", "ucs2", "ujis", "utf16", "utf16le", "utf32",
    "utf8mb3", "utf8mb4",
];

/// Every collation MySQL 8.4.11 has, from its `information_schema.COLLATIONS`.
const COLLATIONS: [&str; 286] = [
    "armscii8_bin",
    "armscii8_general_ci",
    "ascii_bin",
    "ascii_general_ci",
    "big5_bin",
    "big5_chinese_ci",
    "binary",
    "cp1250_bin",
    "cp1250_croatian_ci",
    "cp1250_czech_cs",
    "cp1250_general_ci",
    "cp1250_polish_ci",
    "cp1251_bin",
    "cp1251_bulgarian_ci",
    "cp1251_general_ci",
    "cp1251_general_cs",
    "cp1251_ukrainian_ci",
    "cp1256_bin",
    "cp1256_general_ci",
    "cp1257_bin",
    "cp1257_general_ci",
    "cp1257_lithuanian_ci",
    "cp850_bin",
    "cp850_general_ci",
    "cp852_bin",
    "cp852_general_ci",
    "cp866_bin",
    "cp866_general_ci",
    "cp932_bin",
    "cp932_japanese_ci",
    "dec8_bin",
    "dec8_swedish_ci",
    "eucjpms_bin",
    "eucjpms_japanese_ci",
    "euckr_bin",
    "euckr_korean_ci",
    "gb18030_bin",
    "gb18030_chinese_ci",
    "gb18030_unicode_520_ci",
    "gb2312_bin",
    "gb2312_chinese_ci",
    "gbk_bin",
    "gbk_chinese_ci",
    "geostd8_bin",
    "geostd8_general_ci",
    "greek_bin",
    "greek_general_ci",
    "hebrew_bin",
    "hebrew_general_ci",
    "hp8_bin",
    "hp8_english_ci",
    "keybcs2_bin",
    "keybcs2_general_ci",
    "koi8r_bin",
    "koi8r_general_ci",
    "koi8u_bin",
    "koi8u_general_ci",
    "latin1_bin",
    "latin1_danish_ci",
    "latin1_general_ci",
    "latin1_general_cs",
    "latin1_german1_ci",
    "latin1_german2_ci",
    "latin1_spanish_ci",
    "latin1_swedish_ci",
    "latin2_bin",
    "latin2_croatian_ci",
    "latin2_czech_cs",
    "latin2_general_ci",
    "latin2_hungarian_ci",
    "latin5_bin",
    "latin5_turkish_ci",
    "latin7_bin",
    "latin7_estonian_cs",
    "latin7_general_ci",
    "latin7_general_cs",
    "macce_bin",
    "macce_general_ci",
    "macroman_bin",
    "macroman_general_ci",
    "sjis_bin",
    "sjis_japanese_ci",
    "swe7_bin",
    "swe7_swedish_ci",
    "tis620_bin",
    "tis620_thai_ci",
    "ucs2_bin",
    "ucs2_croatian_ci",
    "ucs2_czech_ci",
    "ucs2_danish_ci",
    "ucs2_esperanto_ci",
    "ucs2_estonian_ci",
    "ucs2_general_ci",
    "ucs2_general_mysql500_ci",
    "ucs2_german2_ci",
    "ucs2_hungarian_ci",
    "ucs2_icelandic_ci",
    "ucs2_latvian_ci",
    "ucs2_lithuanian_ci",
    "ucs2_persian_ci",
    "ucs2_polish_ci",
    "ucs2_romanian_ci",
    "ucs2_roman_ci",
    "ucs2_sinhala_ci",
    "ucs2_slovak_ci",
    "ucs2_slovenian_ci",
    "ucs2_spanish2_ci",
    "ucs2_spanish_ci",
    "ucs2_swedish_ci",
    "ucs2_turkish_ci",
    "ucs2_unicode_520_ci",
    "ucs2_unicode_ci",
    "ucs2_vietnamese_ci",
    "ujis_bin",
    "ujis_japanese_ci",
    "utf16le_bin",
    "utf16le_general_ci",
    "utf16_bin",
    "utf16_croatian_ci",
    "utf16_czech_ci",
    "utf16_danish_ci",
    "utf16_esperanto_ci",
    "utf16_estonian_ci",
    "utf16_general_ci",
    "utf16_german2_ci",
    "utf16_hungarian_ci",
    "utf16_icelandic_ci",
    "utf16_latvian_ci",
    "utf16_lithuanian_ci",
    "utf16_persian_ci",
    "utf16_polish_ci",
    "utf16_romanian_ci",
    "utf16_roman_ci",
    "utf16_sinhala_ci",
    "utf16_slovak_ci",
    "utf16_slovenian_ci",
    "utf16_spanish2_ci",
    "utf16_spanish_ci",
    "utf16_swedish_ci",
    "utf16_turkish_ci",
    "utf16_unicode_520_ci",
    "utf16_unicode_ci",
    "utf16_vietnamese_ci",
    "utf32_bin",
    "utf32_croatian_ci",
    "utf32_czech_ci",
    "utf32_danish_ci",
    "utf32_esperanto_ci",
    "utf32_estonian_ci",
    "utf32_general_ci",
    "utf32_german2_ci",
    "utf32_hungarian_ci",
    "utf32_icelandic_ci",
    "utf32_latvian_ci",
    "utf32_lithuanian_ci",
    "utf32_persian_ci",
    "utf32_polish_ci",
    "utf32_romanian_ci",
    "utf32_roman_ci",
    "utf32_sinhala_ci",
    "utf32_slovak_ci",
    "utf32_slovenian_ci",
    "utf32_spanish2_ci",
    "utf32_spanish_ci",
    "utf32_swedish_ci",
    "utf32_turkish_ci",
    "utf32_unicode_520_ci",
    "utf32_unicode_ci",
    "utf32_vietnamese_ci",
    "utf8mb3_bin",
    "utf8mb3_croatian_ci",
    "utf8mb3_czech_ci",
    "utf8mb3_danish_ci",
    "utf8mb3_esperanto_ci",
    "utf8mb3_estonian_ci",
    "utf8mb3_general_ci",
    "utf8mb3_general_mysql500_ci",
    "utf8mb3_german2_ci",
    "utf8mb3_hungarian_ci",
    "utf8mb3_icelandic_ci",
    "utf8mb3_latvian_ci",
    "utf8mb3_lithuanian_ci",
    "utf8mb3_persian_ci",
    "utf8mb3_polish_ci",
    "utf8mb3_romanian_ci",
    "utf8mb3_roman_ci",
    "utf8mb3_sinhala_ci",
    "utf8mb3_slovak_ci",
    "utf8mb3_slovenian_ci",
    "utf8mb3_spanish2_ci",
    "utf8mb3_spanish_ci",
    "utf8mb3_swedish_ci",
    "utf8mb3_tolower_ci",
    "utf8mb3_turkish_ci",
    "utf8mb3_unicode_520_ci",
    "utf8mb3_unicode_ci",
    "utf8mb3_vietnamese_ci",
    "utf8mb4_0900_ai_ci",
    "utf8mb4_0900_as_ci",
    "utf8mb4_0900_as_cs",
    "utf8mb4_0900_bin",
    "utf8mb4_bg_0900_ai_ci",
    "utf8mb4_bg_0900_as_cs",
    "utf8mb4_bin",
    "utf8mb4_bs_0900_ai_ci",
    "utf8mb4_bs_0900_as_cs",
    "utf8mb4_croatian_ci",
    "utf8mb4_cs_0900_ai_ci",
    "utf8mb4_cs_0900_as_cs",
    "utf8mb4_czech_ci",
    "utf8mb4_danish_ci",
    "utf8mb4_da_0900_ai_ci",
    "utf8mb4_da_0900_as_cs",
    "utf8mb4_de_pb_0900_ai_ci",
    "utf8mb4_de_pb_0900_as_cs",
    "utf8mb4_eo_0900_ai_ci",
    "utf8mb4_eo_0900_as_cs",
    "utf8mb4_esperanto_ci",
    "utf8mb4_estonian_ci",
    "utf8mb4_es_0900_ai_ci",
    "utf8mb4_es_0900_as_cs",
    "utf8mb4_es_trad_0900_ai_ci",
    "utf8mb4_es_trad_0900_as_cs",
    "utf8mb4_et_0900_ai_ci",
    "utf8mb4_et_0900_as_cs",
    "utf8mb4_general_ci",
    "utf8mb4_german2_ci",
    "utf8mb4_gl_0900_ai_ci",
    "utf8mb4_gl_0900_as_cs",
    "utf8mb4_hr_0900_ai_ci",
    "utf8mb4_hr_0900_as_cs",
    "utf8mb4_hungarian_ci",
    "utf8mb4_hu_0900_ai_ci",
    "utf8mb4_hu_0900_as_cs",
    "utf8mb4_icelandic_ci",
    "utf8mb4_is_0900_ai_ci",
    "utf8mb4_is_0900_as_cs",
    "utf8mb4_ja_0900_as_cs",
    "utf8mb4_ja_0900_as_cs_ks",
    "utf8mb4_latvian_ci",
    "utf8mb4_la_0900_ai_ci",
    "utf8mb4_la_0900_as_cs",
    "utf8mb4_lithuanian_ci",
    "utf8mb4_lt_0900_ai_ci",
    "utf8mb4_lt_0900_as_cs",
    "utf8mb4_lv_0900_ai_ci",
    "utf8mb4_lv_0900_as_cs",
    "utf8mb4_mn_cyrl_0900_ai_ci",
    "utf8mb4_mn_cyrl_0900_as_cs",
    "utf8mb4_nb_0900_ai_ci",
    "utf8mb4_nb_0900_as_cs",
    "utf8mb4_nn_0900_ai_ci",
    "utf8mb4_nn_0900_as_cs",
    "utf8mb4_persian_ci",
    "utf8mb4_pl_0900_ai_ci",
    "utf8mb4_pl_0900_as_cs",
    "utf8mb4_polish_ci",
    "utf8mb4_romanian_ci",
    "utf8mb4_roman_ci",
    "utf8mb4_ro_0900_ai_ci",
    "utf8mb4_ro_0900_as_cs",
    "utf8mb4_ru_0900_ai_ci",
    "utf8mb4_ru_0900_as_cs",
    "utf8mb4_sinhala_ci",
    "utf8mb4_sk_0900_ai_ci",
    "utf8mb4_sk_0900_as_cs",
    "utf8mb4_slovak_ci",
    "utf8mb4_slovenian_ci",
    "utf8mb4_sl_0900_ai_ci",
    "utf8mb4_sl_0900_as_cs",
    "utf8mb4_spanish2_ci",
    "utf8mb4_spanish_ci",
    "utf8mb4_sr_latn_0900_ai_ci",
    "utf8mb4_sr_latn_0900_as_cs",
    "utf8mb4_sv_0900_ai_ci",
    "utf8mb4_sv_0900_as_cs",
    "utf8mb4_swedish_ci",
    "utf8mb4_tr_0900_ai_ci",
    "utf8mb4_tr_0900_as_cs",
    "utf8mb4_turkish_ci",
    "utf8mb4_unicode_520_ci",
    "utf8mb4_unicode_ci",
    "utf8mb4_vietnamese_ci",
    "utf8mb4_vi_0900_ai_ci",
    "utf8mb4_vi_0900_as_cs",
    "utf8mb4_zh_0900_as_cs",
];
