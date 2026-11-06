use crate::model::UserRecord;

/// In-memory collection of `UserRecord` values.
#[derive(Default)]
pub struct Store {
    records: Vec<UserRecord>,
}

impl Store {
    pub fn add(&mut self, record: UserRecord) {
        self.records.push(record);
    }

    pub fn find(&self, id: u32) -> Option<&UserRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    pub fn all(&self) -> &[UserRecord] {
        &self.records
    }
}
