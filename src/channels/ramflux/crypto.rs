//! Re-export of the ramflux AEAD. The implementation lives in
//! [`crate::security::ramflux_aead`] so memory does not depend on channels.

pub use crate::security::ramflux_aead::{AccountCrypto, CryptoError, id_hash, overwrite_and_remove};
