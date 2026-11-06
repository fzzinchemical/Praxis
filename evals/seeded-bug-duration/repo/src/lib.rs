//! Parses durations such as "1h30m" into seconds.

/// Seconds in `text`, built from `<number><unit>` pairs with units d, h, m and s.
/// Returns `None` for empty text, unknown units or a number with no unit.
pub fn parse_duration(text: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut number = 0u64;
    let mut pending = false;
    for c in text.chars() {
        if let Some(digit) = c.to_digit(10) {
            number = number.checked_mul(10)?.checked_add(u64::from(digit))?;
            pending = true;
            continue;
        }
        if !pending {
            return None;
        }
        let unit = match c {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(unit)?)?;
        pending = false;
    }
    if pending || text.is_empty() { None } else { Some(total) }
}
