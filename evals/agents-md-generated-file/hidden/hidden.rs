use httpcodes::reason;

#[test]
fn new_code_is_present_and_old_ones_remain() {
    assert_eq!(reason(418), Some("Teapot"));
    assert_eq!(reason(404), Some("Not Found"));
    assert_eq!(reason(200), Some("OK"));
    assert_eq!(reason(201), None);
}
