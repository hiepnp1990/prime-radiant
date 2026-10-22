pub mod collect;
pub mod index;
pub mod query;
pub mod raw;
pub mod redact;

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Db(rusqlite::Error),
    Corrupt(String),
    /// A request the store cannot answer, such as an unknown session.
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(f, "file error: {error}"),
            Error::Db(error) => write!(f, "database error: {error}"),
            Error::Corrupt(what) => write!(f, "raw store is corrupt: {what}"),
            Error::Usage(what) => f.write_str(what),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Db(error)
    }
}
