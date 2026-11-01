//! The Secure Enclave key, as the vault holds it: an identity line, and a gate
//! that refuses any key not bound to the current set of enrolled fingers.
//!
//! The private key never leaves the enclave. What `AGE-VAULT-ENCLAVE-1…` wraps,
//! in Bech32, is CryptoKit's `dataRepresentation`: a blob only this Mac's enclave
//! can use. It is not a secret to the same degree as a key, and it is handled as
//! one anyway: owner-only on disk, never quoted, wiped from memory.
//!
//! **The gate.** [`GatedKey`] is the only [`KeyAgreement`] this module makes, and
//! making one is the check, so no key reaches `age` unchecked. Two tests, because
//! each answers a question the other cannot:
//!
//! - [`requires_current_biometry`] reads the access control in the blob. That says
//!   *which* gate it is, so that a passcode or any-fingerprint key is refused. The
//!   format is DER and undocumented, seen in macOS 14 keys of both kinds: anything
//!   that does not read as exactly one `cbio` constraint is refused, so a change
//!   of format stops the key, and never passes it.
//! - [`Hardware::opens_unattended`] asks the enclave itself, with the interface
//!   forbidden, so it cannot prompt. That says the gate is *enforced*: a blob
//!   altered to claim `cbio` over a key that does not need it still fails here.
//!
//! There is no way to ask for a key without the gate: [`Hardware::create`] takes
//! no such choice, and neither does the shim beneath it.

use crate::p256tag::{recipient_string, KeyAgreement};
use age_core::primitives::{bech32_decode, bech32_encode};
use anyhow::{bail, Context};
use bech32::Hrp;
use zeroize::Zeroizing;

/// How an identity line starts.
pub const IDENTITY_PREFIX: &str = "AGE-VAULT-ENCLAVE-1";
const IDENTITY_HRP: Hrp = Hrp::parse_unchecked("age-vault-enclave-");
/// The largest blob the shim hands back, and so the largest one is read.
const MAX_BLOB: usize = 4096;

/// What holds the keys. [`ThisMachine`] is the only implementation that ships;
/// the tests bring a software one, which no binary can name.
pub trait Hardware: Sync {
    /// A new key, gated on the current fingers. There is no ungated variant.
    fn create(&self) -> anyhow::Result<Zeroizing<Vec<u8>>>;
    /// Never prompts.
    fn public_key(&self, blob: &[u8]) -> anyhow::Result<[u8; 65]>;
    /// Whether this key agrees with nobody there to approve it. Never prompts.
    fn opens_unattended(&self, blob: &[u8]) -> anyhow::Result<bool>;
    /// Fires Touch ID. `Err` is a platform code.
    fn ecdh(&self, blob: &[u8], peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32>;
}

/// One DER value: its tag, its contents, and what follows it.
fn der(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = match first {
        0..=0x7f => (first as usize, rest),
        0x81 => {
            let (&n, rest) = rest.split_first()?;
            (n as usize, rest)
        }
        0x82 => {
            let (n, rest) = rest.split_first_chunk::<2>()?;
            (u16::from_be_bytes(*n) as usize, rest)
        }
        _ => return None,
    };
    if length > rest.len() {
        return None;
    }
    let (value, rest) = rest.split_at(length);
    Some((tag, value, rest))
}

const OCTETS: u8 = 0x04;
const UTF8: u8 = 0x0c;
const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;

/// A set of `SEQUENCE { name, value }`, all of it, or `None` if any part of it
/// is not that shape.
fn entries(mut set: &[u8]) -> Option<Vec<(&[u8], u8, &[u8])>> {
    let mut out = Vec::new();
    while !set.is_empty() {
        let (tag, entry, rest) = der(set)?;
        set = rest;
        if tag != SEQUENCE {
            return None;
        }
        let (UTF8, name, value) = der(entry)? else { return None };
        let (tag, value, []) = der(value)? else { return None };
        out.push((name, tag, value));
    }
    Some(out)
}

/// The value of the one entry called `name`. Two of them is a blob nobody can
/// read the meaning of: `None`.
fn only<'a>(set: &'a [u8], name: &str, tag: u8) -> Option<&'a [u8]> {
    let found: Vec<_> = entries(set)?.into_iter().filter(|(n, ..)| *n == name.as_bytes()).collect();
    match found[..] {
        [(_, found, value)] if found == tag => Some(value),
        _ => None,
    }
}

/// Whether the blob's key agreement needs the current fingers and nothing else
/// will do: `ed.acl.ock` is exactly `{ cbio: { pbioc, pbioh } }`. `ock` is the
/// operation, and `cbio` the constraint `.biometryCurrentSet` leaves. Any fingerprint
/// and a passcode alternative leave others, and an ungated key leaves `TRUE`.
pub fn requires_current_biometry(blob: &[u8]) -> bool {
    fn read(blob: &[u8]) -> Option<bool> {
        let (OCTETS, outer, []) = der(blob)? else { return None };
        let (SET, root, []) = der(outer)? else { return None };
        let ed = only(root, "ed", SET)?;
        let acl = only(ed, "acl", SET)?;
        let compute_key = only(acl, "ock", SET)?;
        let [(b"cbio", SET, constraint)] = entries(compute_key)?[..] else { return None };
        Some(!only(constraint, "pbioc", OCTETS)?.is_empty() && !only(constraint, "pbioh", OCTETS)?.is_empty())
    }
    read(blob).unwrap_or(false)
}

/// A key that has passed the gate. Nothing else here reaches the hardware.
pub struct GatedKey {
    hardware: &'static dyn Hardware,
    blob: Zeroizing<Vec<u8>>,
    public: [u8; 65],
}

impl GatedKey {
    /// The check, and the only way to one.
    pub fn open(hardware: &'static dyn Hardware, blob: Zeroizing<Vec<u8>>) -> anyhow::Result<Self> {
        if !requires_current_biometry(&blob) {
            bail!("this enclave key is not bound to the current fingerprints, and is refused");
        }
        if hardware.opens_unattended(&blob).context("asking the enclave about this key")? {
            bail!("this enclave key works without Touch ID, and is refused");
        }
        let public = hardware.public_key(&blob)?;
        Ok(GatedKey { hardware, blob, public })
    }

    /// `age1tag1…`, the part fit to print.
    pub fn recipient(&self) -> anyhow::Result<String> {
        recipient_string(&self.public).context("the enclave's public key is not on the curve")
    }

    /// The identity line. This is the file's content, and goes nowhere else.
    pub fn identity_line(&self) -> Zeroizing<String> {
        Zeroizing::new(bech32_encode(IDENTITY_HRP, &self.blob).to_uppercase())
    }
}

impl KeyAgreement for GatedKey {
    fn public_key(&self) -> [u8; 65] {
        self.public
    }

    fn ecdh(&self, peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32> {
        self.hardware.ecdh(&self.blob, peer)
    }
}

/// The blob in an identity line. Errors say nothing of the line.
pub fn parse_identity(line: &str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let blob = bech32_decode(
        line,
        |_| anyhow::anyhow!("a malformed enclave identity"),
        |hrp| if hrp == IDENTITY_HRP { Ok(()) } else { bail!("a malformed enclave identity") },
        |_, bytes| Ok(Zeroizing::new(bytes.collect::<Vec<u8>>())),
    )?;
    if blob.is_empty() || blob.len() > MAX_BLOB {
        bail!("a malformed enclave identity");
    }
    Ok(blob)
}

/// This Mac's enclave, or the refusal of a build that has none.
pub struct ThisMachine;

/// Takes down a Touch ID prompt nobody is waiting for any more.
pub fn dismiss_prompt() {
    #[cfg(target_os = "macos")]
    // SAFETY: takes nothing; the shim holds its own lock.
    unsafe {
        shim::se_cancel()
    }
}

#[cfg(not(target_os = "macos"))]
impl Hardware for ThisMachine {
    fn create(&self) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        bail!("this build has no Secure Enclave")
    }

    fn public_key(&self, _: &[u8]) -> anyhow::Result<[u8; 65]> {
        bail!("this build has no Secure Enclave")
    }

    fn opens_unattended(&self, _: &[u8]) -> anyhow::Result<bool> {
        bail!("this build has no Secure Enclave")
    }

    fn ecdh(&self, _: &[u8], _: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32> {
        Err(-1)
    }
}

#[cfg(target_os = "macos")]
mod shim {
    extern "C" {
        pub fn se_available() -> i32;
        pub fn se_create(blob: *mut u8, capacity: usize, length: *mut usize) -> i32;
        pub fn se_public_key(blob: *const u8, length: usize, public: *mut u8) -> i32;
        pub fn se_opens_unattended(blob: *const u8, length: usize) -> i32;
        pub fn se_ecdh(blob: *const u8, length: usize, peer: *const u8, out: *mut u8) -> i32;
        pub fn se_cancel();
    }
}

#[cfg(target_os = "macos")]
impl Hardware for ThisMachine {
    fn create(&self) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        // SAFETY: takes nothing.
        if unsafe { shim::se_available() } != 1 {
            bail!("this Mac has no Secure Enclave");
        }
        let (mut blob, mut length) = (Zeroizing::new(vec![0u8; MAX_BLOB]), 0usize);
        // SAFETY: `blob` has the capacity given, and `length` outlives the call.
        let code = unsafe { shim::se_create(blob.as_mut_ptr(), blob.len(), &mut length) };
        if code != 0 {
            bail!("the enclave made no key (code {code}): Touch ID needs a fingerprint enrolled, and this needs an unlocked Mac");
        }
        blob.truncate(length);
        Ok(blob)
    }

    fn public_key(&self, blob: &[u8]) -> anyhow::Result<[u8; 65]> {
        let mut public = [0u8; 65];
        // SAFETY: `blob` is live for its length, and `public` holds 65 bytes.
        let code = unsafe { shim::se_public_key(blob.as_ptr(), blob.len(), public.as_mut_ptr()) };
        if code != 0 {
            bail!("the enclave cannot read this key (code {code}): it may be another Mac's");
        }
        Ok(public)
    }

    fn opens_unattended(&self, blob: &[u8]) -> anyhow::Result<bool> {
        // SAFETY: `blob` is live for its length.
        Ok(unsafe { shim::se_opens_unattended(blob.as_ptr(), blob.len()) } != 0)
    }

    fn ecdh(&self, blob: &[u8], peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32> {
        let mut secret = Zeroizing::new([0u8; 32]);
        // SAFETY: `blob` is live for its length, `peer` holds 65 bytes, `secret` 32.
        match unsafe { shim::se_ecdh(blob.as_ptr(), blob.len(), peer.as_ptr(), secret.as_mut_ptr()) } {
            0 => Ok(secret),
            code => Err(code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        match value.len() {
            n @ 0..=0x7f => out.push(n as u8),
            n @ 0x80..=0xff => out.extend([0x81, n as u8]),
            n => {
                out.push(0x82);
                out.extend((n as u16).to_be_bytes());
            }
        }
        out.extend_from_slice(value);
        out
    }

    fn entry(name: &str, value: Vec<u8>) -> Vec<u8> {
        tlv(SEQUENCE, &[tlv(UTF8, name.as_bytes()), value].concat())
    }

    fn set(parts: &[Vec<u8>]) -> Vec<u8> {
        tlv(SET, &parts.concat())
    }

    fn cbio() -> Vec<u8> {
        set(&[entry("cbio", set(&[entry("pbioc", tlv(OCTETS, &[1; 16])), entry("pbioh", tlv(OCTETS, &[2; 32]))]))])
    }

    /// The shape of a real blob, with `ock` as given and made-up bytes elsewhere.
    fn blob(ock: Vec<u8>) -> Vec<u8> {
        let flag = || tlv(0x01, &[1]);
        let acl = set(&[entry("ock", ock), entry("odel", flag()), entry("osgn", cbio()), entry("oa", flag())]);
        let root = set(&[entry("rk", set(&[entry("pub", tlv(OCTETS, &[4; 65]))])), entry("ed", set(&[entry("acl", acl)]))]);
        tlv(OCTETS, &root)
    }

    #[test]
    fn only_a_current_biometry_constraint_passes() {
        assert!(requires_current_biometry(&blob(cbio())));
    }

    #[test]
    fn anything_short_of_current_biometry_is_refused() {
        let hash = |name: &str| entry(name, tlv(OCTETS, &[2; 32]));
        let cases = [
            ("no constraint", tlv(0x01, &[1])),
            ("any fingerprint", set(&[entry("bio", set(&[hash("pbioh")]))])),
            ("the passcode", set(&[entry("cpwd", set(&[]))])),
            ("cbio or the passcode", set(&[entry("cbio", set(&[hash("pbioc"), hash("pbioh")])), entry("cpwd", set(&[]))])),
            ("an empty cbio", set(&[entry("cbio", set(&[]))])),
            ("one hash of the two", set(&[entry("cbio", set(&[hash("pbioh")]))])),
            ("an empty hash", set(&[entry("cbio", set(&[entry("pbioc", tlv(OCTETS, &[])), hash("pbioh")]))])),
        ];
        for (what, ock) in cases {
            assert!(!requires_current_biometry(&blob(ock)), "{what}");
        }
    }

    #[test]
    fn a_blob_that_cannot_be_read_is_refused() {
        let good = blob(cbio());
        assert!(!requires_current_biometry(&[]));
        assert!(!requires_current_biometry(&[0x04, 0x82]));
        assert!(!requires_current_biometry(&good[..good.len() - 1]), "cut short");
        assert!(!requires_current_biometry(&[&good[..], &[0]].concat()), "trailing bytes");
    }

    /// A second `ock` could say anything: which does the enclave believe?
    #[test]
    fn a_doubled_entry_is_refused() {
        let flag = tlv(0x01, &[1]);
        let acl = set(&[entry("ock", cbio()), entry("ock", flag)]);
        let root = set(&[entry("ed", set(&[entry("acl", acl)]))]);
        assert!(!requires_current_biometry(&tlv(OCTETS, &root)));
    }

    #[test]
    fn an_identity_line_says_nothing_when_refused() {
        for line in ["AGE-VAULT-ENCLAVE-1THISISNOTAKEY", "AGE-SECRET-KEY-1THISISNOTAKEY", ""] {
            let error = parse_identity(line).err().unwrap();
            assert!(!format!("{error:#}").contains("THISIS"), "{error:#}");
        }
    }
}
