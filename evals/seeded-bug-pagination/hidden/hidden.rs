use pager::{page, page_count};

#[test]
fn every_item_appears_exactly_once_across_pages() {
    for len in 0..25usize {
        for per in 1..7usize {
            let items: Vec<usize> = (0..len).collect();
            let mut seen = Vec::new();
            for p in 1..=page_count(len, per) {
                seen.extend(page(&items, p, per));
            }
            assert_eq!(seen, items, "len={len} per={per}");
        }
    }
}

#[test]
fn page_count_rounds_up_only_when_needed() {
    assert_eq!(page_count(0, 3), 0);
    assert_eq!(page_count(6, 3), 2);
    assert_eq!(page_count(7, 3), 3);
    assert_eq!(page_count(7, 0), 0);
}

#[test]
fn out_of_range_pages_are_empty() {
    assert!(page(&[1, 2], 0, 2).is_empty());
    assert!(page(&[1, 2], 2, 2).is_empty());
    assert!(page::<i32>(&[], 1, 2).is_empty());
}
