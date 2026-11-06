/// One person known to the system.
#[derive(Clone, Debug, PartialEq)]
pub struct UserRecord {
    pub id: u32,
    pub name: String,
}

impl UserRecord {
    pub fn new(id: u32, name: &str) -> UserRecord {
        UserRecord { id, name: name.to_string() }
    }
}
