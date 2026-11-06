use wordtools::headline::headline;
use wordtools::text::title_case;

#[test]
fn text_module_replaces_util() {
    assert_eq!(title_case("hello big world"), "Hello Big World");
    assert_eq!(headline("good news"), "Good News!");
}
