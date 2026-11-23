//! Error types for the mcsv_manager crate.
//!
//! This module defines the [`Error`] enum, a unified error type used throughout the client.  
//! All errors produced by this crate will be returned as a variant of [`Error`],  
//! making error handling simple and consistent.

use std::fmt::{self, Display};

/// A unified error type for all operations in this crate.
///
/// Most methods return a [`Result<T, Error>`]. This enum wraps errors from
/// underlying libraries and provides an additional variant for HTTP status-based errors.
#[derive(Debug)]
pub enum Error {
    /// A connection error from the `http_unix_client` crate.
    ConnectionError(http_unix_client::Error),
    /// Insufficient Permission
    Forbidden,
    /// Service Failure
    ServiceFailed,
    /// Server ID is not found.
    NotFound,
    /// An error from serializing or deserializing JSON (available when the `json` feature is enabled).
    Decode(serde_json::Error)
}

impl Error {
    /// Returns true if the error is from a type Builder.
    pub fn is_connection(&self) -> bool {
        matches!(self, Self::ConnectionError(..))
    }

    /// Returns true if the error is from `Response::error_for_status`.
    pub fn is_permission(&self) -> bool {
        matches!(self, Self::Forbidden { .. })
    }
}

impl From<http_unix_client::Error> for Error {
    fn from(value: http_unix_client::Error) -> Self {
        Self::ConnectionError(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Decode(value)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionError(e) => e.fmt(f)?,
            Self::Forbidden => write!(f, "Insufficient Permission")?,
            Self::ServiceFailed => write!(f, "Request Failed")?,
            Self::NotFound => write!(f, "Invalid Server ID")?,
            Self::Decode(e) => {
                write!(f, "Error decoding response body.")?;
                e.fmt(f)?
            },
        };
        Ok(())
    }
}

impl std::error::Error for Error {}

/// A result type alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
