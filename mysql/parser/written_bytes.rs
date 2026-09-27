//! What `ASCII`, `ORD`, `CRC32`, `QUOTE` and `TO_BASE64` answer for the bytes
//! of a value, which the engine has no calls for.
//!
//! A number reaches each of these written out, the way MySQL writes it before
//! it reads the bytes. Everything here was measured on MySQL 8.4.11.

/// The first byte, which `ASCII` answers, and 0 for an empty word. Measured:
/// `ASCII('Ünï')` is 195, the first of the two bytes `Ü` is written in.
pub fn first_byte(bytes: &[u8]) -> i64 {
    bytes.first().map_or(0, |byte| i64::from(*byte))
}

/// The bytes of the first character read as one number, the first byte
/// leading, which `ORD` answers, and 0 for an empty word. Measured:
/// `ORD('Ünï')` is 50076, 0xC39C.
pub fn first_character_code(text: &str) -> i64 {
    let Some(first) = text.chars().next() else {
        return 0;
    };
    let mut written = [0; 4];
    first
        .encode_utf8(&mut written)
        .bytes()
        .fold(0, |code, byte| code * 256 + i64::from(byte))
}

/// The CRC-32 checksum `CRC32` answers, the one zlib and Ethernet use.
/// Measured: `CRC32('hello')` is 907060870.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut checksum = !0u32;
    for byte in bytes {
        checksum ^= u32::from(*byte);
        for _ in 0..8 {
            let low_bit = checksum & 1;
            checksum >>= 1;
            if low_bit == 1 {
                checksum ^= 0xEDB8_8320;
            }
        }
    }
    !checksum
}

/// The word `QUOTE` answers: the value in single quotes, with a backslash
/// before a backslash and a quote, `\0` for a NUL and `\Z` for a Ctrl-Z, and
/// every other byte as it stands. A NULL answers the word `NULL` without
/// quotes. Measured: `QUOTE('a''b\\c')` is `'a\'b\\c'`, and a newline, a
/// carriage return and a tab go in unescaped.
pub fn quoted_for_sql(text: Option<&str>) -> String {
    let Some(text) = text else {
        return "NULL".to_owned();
    };
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for character in text.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            '\0' => quoted.push_str("\\0"),
            '\u{1a}' => quoted.push_str("\\Z"),
            other => quoted.push(other),
        }
    }
    quoted.push('\'');
    quoted
}

/// The base64 `TO_BASE64` answers, with a newline after every 76 characters
/// of it. Measured: 57 bytes answer 76 characters and 58 answer 81, one of
/// them the newline.
pub fn to_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = Vec::with_capacity(base64_length(bytes.len() as u64) as usize);
    let mut line = 0;
    for chunk in bytes.chunks(3) {
        if line == 76 {
            encoded.push(b'\n');
            line = 0;
        }
        let triple = chunk.iter().enumerate().fold(0u32, |triple, (at, byte)| {
            triple | (u32::from(*byte) << (16 - 8 * at))
        });
        for at in 0..4 {
            encoded.push(if at <= chunk.len() {
                ALPHABET[((triple >> (18 - 6 * at)) & 63) as usize]
            } else {
                b'='
            });
        }
        line += 4;
    }
    String::from_utf8(encoded).expect("the base64 alphabet is ASCII")
}

/// How many characters `TO_BASE64` answers for that many bytes, the newlines
/// counted. Measured: 56 bytes answer 76 and 60 answer 81.
pub fn base64_length(bytes: u64) -> u64 {
    let encoded = bytes.div_ceil(3) * 4;
    encoded + encoded.saturating_sub(1) / 76
}
