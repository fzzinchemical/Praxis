#[derive(Debug, PartialEq)]
pub enum Error {
    /// The input text could not be used.
    Invalid(String),
}
