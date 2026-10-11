use std::fmt;

/// Everything that can go wrong while running a statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The SQL text did not parse.
    Parse(String),
    /// Valid SQL that Fishbowl does not implement yet.
    Unsupported(String),
    /// A name that does not resolve: unknown table or column, duplicate table.
    Catalog(String),
    /// A rule the data must follow, such as NOT NULL.
    Constraint(String),
    /// The statement did more work than the configured row budget allows.
    BudgetExceeded(u64),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(msg) => write!(f, "parse error: {msg}"),
            Error::Unsupported(what) => write!(f, "not supported yet: {what}"),
            Error::Catalog(msg) => f.write_str(msg),
            Error::Constraint(msg) => write!(f, "constraint failed: {msg}"),
            Error::BudgetExceeded(limit) => {
                write!(f, "statement exceeded the budget of {limit} rows")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn unsupported<T>(what: impl fmt::Display) -> Result<T> {
    Err(Error::Unsupported(what.to_string()))
}
