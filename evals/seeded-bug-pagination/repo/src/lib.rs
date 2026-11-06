//! Slices lists into pages. Pages are numbered from 1.

/// Items on `page`, or nothing if the page is out of range.
pub fn page<T: Clone>(items: &[T], page: usize, per_page: usize) -> Vec<T> {
    if page == 0 || per_page == 0 {
        return Vec::new();
    }
    let start = (page - 1) * per_page;
    if start >= items.len() {
        return Vec::new();
    }
    let end = (start + per_page).min(items.len() - 1);
    items[start..end].to_vec()
}

/// Number of pages needed to show every item.
pub fn page_count(len: usize, per_page: usize) -> usize {
    if per_page == 0 {
        return 0;
    }
    len / per_page
}
