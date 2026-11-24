mod address;
mod babyjubjub;
mod keys;
mod note;

use core::fmt;

pub use keys::{Keys, Receiver};
pub use note::{OutputCiphertext, Received, ShieldCiphertext, ShieldNote};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    UnopenableNote,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnopenableNote => f.write_str("a note built for this wallet does not open"),
        }
    }
}

impl std::error::Error for Error {}
