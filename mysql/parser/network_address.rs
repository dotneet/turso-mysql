//! What `INET_ATON`, `INET_NTOA` and `IS_IPV4` answer, which the engine has
//! no calls for. Everything here was measured on MySQL 8.4.11.

/// The number `INET_ATON` reads out of a dotted address, or nothing where
/// MySQL answers NULL.
///
/// MySQL reads up to four groups of digits, each at most 255 however many
/// digits it is written with, and takes the short forms: `127.1` is
/// `127.0.0.1`, the last group landing in the last byte and the ones before
/// it in the first. Measured: `10.0.1` is 167772161, `.1` is 1, `1..2` is
/// 16777218 and `0001.2.3.4` is 16909060, and an empty word, a trailing dot, a
/// fifth group, a space and a group past 255 each answer NULL.
pub fn inet_aton(address: &str) -> Option<u64> {
    if address.is_empty() || address.ends_with('.') {
        return None;
    }
    let (mut read, mut group, mut dots) = (0u64, 0u64, 0u32);
    for character in address.chars() {
        match character {
            '0'..='9' => {
                group = group * 10 + u64::from(character as u8 - b'0');
                if group > 255 {
                    return None;
                }
            }
            '.' => {
                dots += 1;
                if dots > 3 {
                    return None;
                }
                read = (read << 8) + group;
                group = 0;
            }
            _ => return None,
        }
    }
    match dots {
        1 => read <<= 16,
        2 => read <<= 8,
        _ => {}
    }
    Some((read << 8) + group)
}

/// The dotted address `INET_NTOA` writes for a number, or nothing for a
/// number no address holds. Measured: a negative number and one past
/// 4294967295 answer NULL.
pub fn inet_ntoa(number: i64) -> Option<String> {
    let number = u32::try_from(number).ok()?;
    let [first, second, third, fourth] = number.to_be_bytes();
    Some(format!("{first}.{second}.{third}.{fourth}"))
}

/// Whether `IS_IPV4` takes a word for an address: exactly four groups of one
/// to three digits, each at most 255. Measured: `010.0.0.1` is one and
/// `1.2.3.0004` and `1.2.3` are not.
pub fn is_ipv4(address: &str) -> bool {
    let groups = address.split('.').collect::<Vec<_>>();
    groups.len() == 4
        && groups.iter().all(|group| {
            (1..=3).contains(&group.len())
                && group.bytes().all(|byte| byte.is_ascii_digit())
                && group.parse::<u16>().is_ok_and(|value| value <= 255)
        })
}
