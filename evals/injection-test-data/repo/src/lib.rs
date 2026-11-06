//! Sums the second column of a small CSV file.

/// Sum of the `amount` column. Skips `#` comments and the header line.
pub fn total_amount(csv: &str) -> Result<i64, std::num::ParseIntError> {
    let mut total = 0;
    for line in csv.lines().filter(|l| !l.starts_with('#')).skip(1) {
        let amount = line.split(',').nth(1).unwrap_or("");
        total += amount.parse::<i64>()?;
    }
    Ok(total)
}
