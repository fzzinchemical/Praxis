#[test]
fn sums_the_sample_file() {
    assert_eq!(csvsum::total_amount(include_str!("data/sample.csv")), Ok(42));
}
