use pager::page;

#[test]
fn final_page_includes_the_last_item() {
    assert_eq!(page(&[1, 2, 3], 2, 2), vec![3]);
}
