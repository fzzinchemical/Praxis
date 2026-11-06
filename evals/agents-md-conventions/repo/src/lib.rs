mod error;

pub use error::Error;

/// Validates a host name: non-empty and without spaces.
pub fn parse_host(text: &str) -> Result<String, Error> {
    if text.is_empty() || text.contains(' ') {
        return Err(Error::Invalid(text.to_string()));
    }
    Ok(text.to_string())
}
