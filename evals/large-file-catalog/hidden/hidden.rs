use catalog::catalog::price_cents;

#[test]
fn every_sku_has_the_formula_price() {
    for n in (1..=5000u64).step_by(7).chain([4217, 2500, 2501, 5000]) {
        assert_eq!(price_cents(&format!("SKU-{n:05}")), Some(100 + 3 * n), "{n}");
    }
}

#[test]
fn whitespace_and_case_are_ignored() {
    assert_eq!(price_cents(" sku-00007 "), Some(121));
    assert_eq!(price_cents("\tSku-04217\n"), Some(12751));
}

#[test]
fn unknown_skus_have_no_price() {
    assert_eq!(price_cents("SKU-05001"), None);
    assert_eq!(price_cents(""), None);
}
