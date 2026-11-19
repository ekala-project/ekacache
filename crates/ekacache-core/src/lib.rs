//! `ekacache-core`: everything that jig and jigd must agree on byte for byte.
//!
//! Specified by `behaviors/jig-keys.md` (§The Hasher, §Key types and prefixes, §Store paths,
//! §Key: lexical normalisation, §Manifest), `behaviors/jig-cache-client.md` (§Value encoding,
//! §Version negotiation) and `behaviors/jigd-protocol.md` (§Framing, §Tokenization,
//! §Number parsing, §HELLO). See `implementation/02-core.md`.
//!
//! Paths, keys and manifest lines are byte strings end to end: legacy never validates UTF-8.

#![deny(unsafe_code)]

pub mod bytes;
pub mod env;
#[cfg(feature = "frame")]
pub mod frame;
pub mod fspath;
pub mod hasher;
pub mod keys;
pub mod manifest;
pub mod store_path;
pub mod wire;

pub use hasher::{Digest, Hasher, hash_of};
