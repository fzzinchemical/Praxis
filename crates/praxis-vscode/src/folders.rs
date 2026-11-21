//! Folder rules for the Sessions view, kept free of VS Code so they are unit tested: which folders
//! the view lists, and which CLI calls a drop, a pick or a new folder asks for.

use crate::protocol::{SessionItem, folder_args};
use std::collections::BTreeSet;

/// Every folder the view lists, each once and sorted: the folders created on purpose and the
/// folders that hold a session.
pub fn folder_list(items: &[SessionItem], created: &[String]) -> Vec<String> {
    let names: BTreeSet<&str> = items
        .iter()
        .filter_map(|item| item.folder.as_deref())
        .chain(created.iter().map(String::as_str))
        .collect();
    names.into_iter().map(str::to_owned).collect()
}

/// The CLI calls that file each session in `ids` under `folder`, or take it out of its folder when
/// `folder` is `None`. Malformed ids get no call.
pub fn move_args(ids: &[String], folder: Option<&str>) -> Vec<Vec<String>> {
    ids.iter()
        .filter_map(|id| folder_args(id, folder.unwrap_or("")))
        .collect()
}

/// The CLI call that creates the empty folder `name`, or `None` for a blank name. `--new-folder=`
/// keeps a name that starts with `-` from being read as a flag.
pub fn create_args(name: &str) -> Option<Vec<String>> {
    let name = name.trim();
    (!name.is_empty()).then(|| vec!["sessions".to_owned(), format!("--new-folder={name}")])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, folder: Option<&str>) -> SessionItem {
        SessionItem {
            id: id.to_owned(),
            label: id.to_owned(),
            folder: folder.map(str::to_owned),
        }
    }

    #[test]
    fn folders_list_once_each_sorted_with_created_ones_included() {
        let items = [
            item("1700000000-1", Some("Work")),
            item("1700000000-2", None),
            item("1700000000-3", Some("Home")),
            item("1700000000-4", Some("Work")),
        ];
        let created = ["Empty".to_owned(), "Home".to_owned()];
        assert_eq!(folder_list(&items, &created), ["Empty", "Home", "Work"]);
    }

    #[test]
    fn a_created_folder_lists_with_no_session_in_it() {
        assert_eq!(folder_list(&[], &["Idle".to_owned()]), ["Idle"]);
        assert!(folder_list(&[], &[]).is_empty());
    }

    #[test]
    fn a_drop_moves_every_dragged_session_and_skips_malformed_ids() {
        let ids = [
            "1700000000-1".to_owned(),
            "../secret".to_owned(),
            "1700000000-2".to_owned(),
        ];
        let calls = move_args(&ids, Some("Work"));
        assert_eq!(
            calls,
            [
                folder_args("1700000000-1", "Work").unwrap(),
                folder_args("1700000000-2", "Work").unwrap(),
            ]
        );
    }

    #[test]
    fn a_drop_on_the_root_takes_the_sessions_out_of_their_folder() {
        let calls = move_args(&["1700000000-1".to_owned()], None);
        assert_eq!(calls, [folder_args("1700000000-1", "").unwrap()]);
        assert!(calls[0].ends_with(&["1700000000-1".to_owned(), String::new()]));
    }

    #[test]
    fn a_new_folder_needs_a_name_and_keeps_a_leading_dash() {
        assert_eq!(create_args("   "), None);
        assert_eq!(
            create_args(" -Work "),
            Some(vec!["sessions".to_owned(), "--new-folder=-Work".to_owned()])
        );
    }
}
