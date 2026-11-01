//! What crosses the wire, and the rules both sides apply to it. The approver
//! applies them again: it does not trust the server either.

use serde::{Deserialize, Serialize};

/// The Plexus service's name.
pub const SERVICE: &str = "hyperforge-vault";
/// The activation's namespace: `vault.put`, `vault.request`, ….
pub const NAMESPACE: &str = "vault";

/// The largest ciphertext or sealed result accepted.
pub const MAX_CIPHERTEXT: usize = 1024 * 1024;
pub const MAX_PURPOSE: usize = 300;
pub const MAX_CALLER: usize = 120;
/// Requests pending at once: past this a tailnet device cannot pile on more.
pub const MAX_PENDING: usize = 64;

/// A request as the approver lists it: no ciphertext.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(schemars::JsonSchema))]
pub struct RequestSummary {
    pub id: String,
    pub secret: String,
    /// Written by the caller; nothing checks it.
    pub purpose: String,
    /// Written by the caller; nothing checks it.
    pub caller: String,
    pub one_time_pubkey: String,
    /// Milliseconds since the Unix epoch.
    pub created_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    /// Not answered yet.
    Pending,
    /// Sealed to the request's one-time key; this read was the only one.
    Sealed,
    Denied,
    Expired,
}

/// One audit line. Never a value, never ciphertext.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(schemars::JsonSchema))]
pub struct AuditEntry {
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    /// put, request, approve, deny, fulfill, deliver or expire.
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
}

/// Every method's events; refusals are in band, as `Error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VaultEvent {
    /// `put` kept the ciphertext.
    Stored { name: String },
    /// One stored secret, from `list`.
    Secret { name: String, size: u64, stored_at: u64 },
    /// `request` opened a request.
    Requested { id: String, expires_at: u64 },
    /// A pending request, from `pending` or `watch`.
    Pending {
        #[serde(flatten)]
        request: RequestSummary,
    },
    /// `get_request`: the details and the ciphertext, for the approver.
    Request {
        #[serde(flatten)]
        request: RequestSummary,
        ciphertext: String,
    },
    /// `fulfill` kept the sealed result.
    Fulfilled { id: String },
    /// `deny` closed the request.
    Denied { id: String },
    /// `result`.
    Result {
        id: String,
        status: ResultStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sealed: Option<String>,
    },
    /// One line of the audit log.
    Audit {
        #[serde(flatten)]
        entry: AuditEntry,
    },
    /// A call refused, in band.
    Error { message: String },
}

impl plexus_client::FromClientError for VaultEvent {
    fn from_client_error(error: plexus_client::ClientError) -> Self {
        VaultEvent::Error { message: error.to_string() }
    }
}

/// `name` is safe as a file's stem: ASCII, no separators, no leading dot.
pub fn valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Parses as an age file (armored or binary): a header, a MAC'd structure, a
/// nonce. No key is involved, so nothing here can decrypt anything.
pub fn is_age_format(text: &str) -> bool {
    age::Decryptor::new_buffered(age::armor::ArmoredReader::new(text.as_bytes())).is_ok()
}

/// An X25519 age recipient. Everything else is refused: the owner seals to this
/// string, and it comes from the network.
pub fn valid_one_time_pubkey(key: &str) -> bool {
    key.starts_with("age1") && key.parse::<age::x25519::Recipient>().is_ok()
}

/// `text` fit to show a human: no control characters, nothing that reorders
/// the line, bounded. What a caller or a server writes must not be able to
/// move the cursor, retitle the terminal or forge a line of the prompt.
pub fn printable(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .map(|c| match c {
            c if c.is_control() => '?',
            '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '?',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_file_stems() {
        for good in ["a", "db-password", "a.b_c-1"] {
            assert!(valid_secret_name(good), "{good}");
        }
        for bad in ["", ".a", "-a", "a/b", "../a", "a b", "a\n", &"x".repeat(65)] {
            assert!(!valid_secret_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_key_that_could_be_a_flag_is_refused() {
        assert!(valid_one_time_pubkey(&age::x25519::Identity::generate().to_public().to_string()));
        for bad in ["-o/etc/passwd", "--help", "age1", "age1se1qqqq", ""] {
            assert!(!valid_one_time_pubkey(bad), "{bad}");
        }
    }

    #[test]
    fn printable_removes_what_a_terminal_obeys() {
        assert_eq!(printable("a\x1b[2Jb\nc\u{202e}d", 100), "a?[2Jb?c?d");
        assert_eq!(printable("héllo", 2), "hé");
    }
}
