//! The approver's own record of what it enrolled: secret name to the sha256 of
//! its ciphertext. It lives with the owner and never crosses the wire, so a
//! server cannot speak for it. Before any decrypt the approver holds the bytes
//! it was served against this, and a name with no entry, a file it cannot
//! read, or a hash that differs all come to the same thing: nothing is
//! decrypted. A compromised server cannot get one secret opened under
//! another's approved name, nor an older ciphertext under the current one.

use crate::wire::{printable, valid_secret_name};
use anyhow::{bail, Context};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The file's shape: `{"<name>": "<sha256 hex>", …}`.
type Entries = BTreeMap<String, String>;

/// The sha256 of `ciphertext`'s bytes, in lowercase hex.
pub fn digest(ciphertext: &str) -> String {
    Sha256::digest(ciphertext.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn load(path: &Path) -> anyhow::Result<Entries> {
    let text = fs::read_to_string(path).with_context(|| format!("reading the manifest {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing the manifest {}", path.display()))
}

/// Ok only if `ciphertext` is what was enrolled as `name`.
pub fn verify(path: &Path, name: &str, ciphertext: &str) -> anyhow::Result<()> {
    let entries = load(path)?;
    let shown = printable(name, 64);
    let Some(recorded) = entries.get(name) else {
        bail!("the manifest has no entry for '{shown}'");
    };
    if *recorded != digest(ciphertext) {
        bail!("the ciphertext served for '{shown}' does not match the manifest");
    }
    Ok(())
}

/// Records `ciphertext` as `name`, replacing any earlier entry (rotation). The
/// manifest is created owner-only and replaced whole. Call this only with
/// ciphertext the server has just accepted: an entry for a store that was
/// refused would lock the real secret out.
pub fn record(path: &Path, name: &str, ciphertext: &str) -> anyhow::Result<()> {
    if !valid_secret_name(name) {
        bail!("'{}' is not a secret name", printable(name, 70));
    }
    let mut entries = if path.exists() { load(path)? } else { Entries::new() };
    entries.insert(name.to_string(), digest(ciphertext));
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(&entries)?)?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    File::open(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn it_holds_what_was_recorded_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        record(&path, "a", "ciphertext of a").unwrap();
        record(&path, "b", "ciphertext of b").unwrap();
        verify(&path, "a", "ciphertext of a").unwrap();
        verify(&path, "b", "ciphertext of b").unwrap();
        assert!(verify(&path, "a", "ciphertext of b").is_err());
        assert!(verify(&path, "c", "ciphertext of a").is_err());
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn rotation_replaces_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        record(&path, "a", "old").unwrap();
        record(&path, "a", "new").unwrap();
        verify(&path, "a", "new").unwrap();
        assert!(verify(&path, "a", "old").is_err());
    }

    #[test]
    fn a_manifest_it_cannot_read_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        assert!(verify(&path, "a", "x").is_err(), "a missing manifest");
        fs::write(&path, "not json").unwrap();
        assert!(verify(&path, "a", "x").is_err(), "a corrupt manifest");
        assert!(record(&path, "a", "x").is_err(), "a corrupt manifest is not silently replaced");
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn a_bad_name_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        assert!(record(&dir.path().join("m.json"), "../x", "c").is_err());
    }
}
