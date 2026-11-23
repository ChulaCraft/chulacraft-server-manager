pub mod models;

pub(crate) mod systemd1;

pub mod client;

mod error;

pub use error::{Error, Result};
