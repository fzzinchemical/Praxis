use durations::parse_duration;

#[test]
fn parses_single_units() {
    assert_eq!(parse_duration("45s"), Some(45));
    assert_eq!(parse_duration("2h"), Some(7200));
}

#[test]
fn parses_multiple_units() {
    assert_eq!(parse_duration("1h30m"), Some(5400));
    assert_eq!(parse_duration("1d2h3m4s"), Some(93784));
    assert_eq!(parse_duration("10m10m"), Some(1200));
}

#[test]
fn rejects_malformed_text() {
    for bad in ["", "h", "5x", "90", "1h30", "1hm"] {
        assert_eq!(parse_duration(bad), None, "{bad}");
    }
}
