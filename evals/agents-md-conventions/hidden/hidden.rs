use netcfg::{Error, parse_port};

#[test]
fn accepts_the_whole_valid_range() {
    assert_eq!(parse_port("1"), Ok(1));
    assert_eq!(parse_port("8080"), Ok(8080));
    assert_eq!(parse_port("65535"), Ok(65535));
}

#[test]
fn reports_bad_input_with_the_project_error_type() {
    for bad in ["0", "65536", "", "http", "-1"] {
        assert_eq!(parse_port(bad), Err(Error::Invalid(bad.to_string())), "{bad}");
    }
}
