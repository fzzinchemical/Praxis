use crate::UserRecord;

pub fn summary(records: &[UserRecord]) -> String {
    let names: Vec<&str> = records.iter().map(|r: &UserRecord| r.name.as_str()).collect();
    names.join(", ")
}
