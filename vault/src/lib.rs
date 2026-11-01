//! hyperforge-vault: a server that holds only `age` ciphertext, and hands a secret
//! out only after its owner approves that one request. See README.md.

pub mod client;
#[cfg(feature = "server")]
pub mod server;
pub mod wire;
