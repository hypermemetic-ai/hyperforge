//! Keys and encryption, in process, through the `age` crate. No `age` binary and
//! no plugin runs: a key type this build does not know is refused, loudly.
//!
//! [`KeyFormat`] is the seam. Each format reads the lines of a recipients file
//! and of an identity file into the crate's own traits, and [`encrypt`] and
//! [`reseal`] see only those traits. A new key type is one more format in
//! [`FORMATS`], and no change here.

use age::secrecy::ExposeSecret;
use anyhow::{bail, Context};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

/// What one [`pipe`] holds at a time: age's own chunk.
const BUFFER: usize = 64 * 1024;
/// An identity file is a few lines. Sized up front, as in `client::open`: a
/// buffer that grew would leave unzeroized copies of the key behind.
const MAX_IDENTITY_FILE: usize = 64 * 1024;

/// One kind of key. A line that is not this format's is `None`, so the next
/// format gets it; `Some(Err)` is this format's line, and bad.
///
/// An identity may block, for Touch ID say: it is only ever used on
/// [`with_deadline`]'s thread. Neither trait object need be `Send`.
pub trait KeyFormat: Sync {
    fn recipient(&self, line: &str) -> Option<anyhow::Result<Box<dyn age::Recipient>>>;
    fn identity(&self, line: &str) -> Option<anyhow::Result<Box<dyn age::Identity>>>;
}

/// `age1…` and `AGE-SECRET-KEY-1…`.
pub struct X25519;

impl KeyFormat for X25519 {
    fn recipient(&self, line: &str) -> Option<anyhow::Result<Box<dyn age::Recipient>>> {
        // Other recipients start `age1` too: only a parse says this one is ours.
        let recipient = line.parse::<age::x25519::Recipient>().ok()?;
        Some(Ok(Box::new(recipient)))
    }

    fn identity(&self, line: &str) -> Option<anyhow::Result<Box<dyn age::Identity>>> {
        if !line.starts_with("AGE-SECRET-KEY-1") {
            return None;
        }
        // Not the parse error: it may quote the line, and the line is the key.
        Some(match line.parse::<age::x25519::Identity>() {
            Ok(identity) => Ok(Box::new(identity)),
            Err(_) => Err(anyhow::anyhow!("a malformed X25519 identity")),
        })
    }
}

/// What this build reads. The Secure Enclave format goes here when it exists.
pub const FORMATS: &[&dyn KeyFormat] = &[&X25519];

fn key_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().enumerate().map(|(i, l)| (i + 1, l.trim())).filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
}

/// Every recipient in a recipients file, or an error. Nothing is skipped: a
/// recipient left out is a key that later cannot open the secret.
pub fn parse_recipients(text: &str, formats: &[&dyn KeyFormat]) -> anyhow::Result<Vec<Box<dyn age::Recipient>>> {
    let mut recipients = Vec::new();
    for (number, line) in key_lines(text) {
        match formats.iter().find_map(|f| f.recipient(line)) {
            Some(recipient) => recipients.push(recipient.with_context(|| format!("line {number}"))?),
            None => bail!("line {number}: not a recipient this build can use. Nothing was encrypted"),
        }
    }
    Ok(recipients)
}

/// Every identity in a file. Errors name lines, never their text.
pub fn load_identities(path: &Path, formats: &[&dyn KeyFormat]) -> anyhow::Result<Vec<Box<dyn age::Identity>>> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut text = Zeroizing::new(Vec::with_capacity(MAX_IDENTITY_FILE));
    file.take(MAX_IDENTITY_FILE as u64).read_to_end(&mut text).with_context(|| format!("reading {}", path.display()))?;
    if text.len() == MAX_IDENTITY_FILE {
        bail!("{} is too long for an identity file", path.display());
    }
    let text = std::str::from_utf8(&text).map_err(|_| anyhow::anyhow!("{} is not text", path.display()))?;
    let mut identities = Vec::new();
    for (number, line) in key_lines(text) {
        match formats.iter().find_map(|f| f.identity(line)) {
            Some(identity) => identities.push(identity.with_context(|| format!("{} line {number}", path.display()))?),
            None => bail!("{} line {number}: not an identity this build can use", path.display()),
        }
    }
    if identities.is_empty() {
        bail!("{} holds no identity", path.display());
    }
    Ok(identities)
}

/// Moves `from` to `to` through one zeroizing buffer, so nothing accumulates.
fn pipe(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<()> {
    let mut buffer = Zeroizing::new(vec![0u8; BUFFER]);
    loop {
        match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => to.write_all(&buffer[..n])?,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Armored age, for everyone in `recipients`, of everything in `plain`.
pub fn encrypt(recipients: &[Box<dyn age::Recipient>], plain: &mut impl Read) -> anyhow::Result<String> {
    use age::armor::{ArmoredWriter, Format};
    let encryptor = age::Encryptor::with_recipients(recipients.iter().map(|r| r.as_ref())).context("the recipients")?;
    let armor = ArmoredWriter::wrap_output(Vec::new(), Format::AsciiArmor)?;
    let mut stream = encryptor.wrap_output(armor)?;
    pipe(plain, &mut stream).context("encrypting")?;
    let armored = stream.finish()?.finish()?;
    String::from_utf8(armored).context("armored output is not text")
}

/// Opens `ciphertext` with the identities in `identity_file` and seals what is
/// inside to `one_time_key` alone. The identity file is read here, at each
/// decrypt, so nothing holds a key between requests. The seal is returned only
/// after the last chunk authenticated: a truncated file yields no seal at all.
pub fn reseal(identity_file: &Path, formats: &[&dyn KeyFormat], ciphertext: &str, one_time_key: &str) -> anyhow::Result<String> {
    // Checked here as well: the server is not trusted to have checked.
    if !crate::wire::valid_one_time_pubkey(one_time_key) {
        bail!("the one-time key is not an X25519 age recipient");
    }
    let one_time: Box<dyn age::Recipient> = Box::new(one_time_key.parse::<age::x25519::Recipient>().map_err(|e| anyhow::anyhow!(e))?);
    let reader = age::armor::ArmoredReader::new(ciphertext.as_bytes());
    let decryptor = age::Decryptor::new_buffered(reader).context("the ciphertext is not age format")?;
    let identities = load_identities(identity_file, formats)?;
    let mut plain = decryptor.decrypt(identities.iter().map(|i| i.as_ref())).context("the ciphertext does not open with this identity")?;
    encrypt(std::slice::from_ref(&one_time), &mut plain)
}

/// The recovery path, for whoever holds the offline key: `ciphertext` opened with
/// `identity_file` if the manifest holds it as `name`, and the plaintext handed to
/// the caller, who decides where it goes. The hash is checked before the identity
/// is read, and a value comes back only once its last chunk has authenticated, so
/// a cut file gives an error and no part of the value.
pub fn recover(manifest: &Path, name: &str, identity_file: &Path, formats: &[&dyn KeyFormat], ciphertext: &str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    crate::manifest::verify(manifest, name, ciphertext)?;
    let reader = age::armor::ArmoredReader::new(ciphertext.as_bytes());
    let decryptor = age::Decryptor::new_buffered(reader).context("the ciphertext is not age format")?;
    let identities = load_identities(identity_file, formats)?;
    let mut plain = decryptor.decrypt(identities.iter().map(|i| i.as_ref())).context("the ciphertext does not open with this identity")?;
    // Sized up front: a Vec that grew would leave unzeroized copies behind.
    let mut value = Zeroizing::new(Vec::with_capacity(ciphertext.len()));
    plain.read_to_end(&mut value).context("reading the ciphertext: it is cut short or corrupt, and nothing is returned")?;
    Ok(value)
}

/// Runs `work`, which may block on the owner, on its own thread, and stops
/// waiting after `limit`. A thread and not `spawn_blocking`: the runtime would
/// wait for that at exit, and `--once` must be able to give up.
pub async fn with_deadline<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    tokio::time::timeout(limit, finished).await.context("the identity took too long")?.context("the decrypt died")?
}

/// Writes a new X25519 identity to `path`, in the format `age-keygen` writes:
/// owner-only, and never over a file already there. Returns the recipient, the
/// one part fit to print.
pub fn generate_recovery_identity(path: &Path) -> anyhow::Result<String> {
    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public().to_string();
    let created = humantime::format_rfc3339_seconds(SystemTime::now());
    let secret = identity.to_string();
    let text = Zeroizing::new(format!("# created: {created}\n# public key: {recipient}\n{}\n", secret.expose_secret()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let written = file.write_all(text.as_bytes()).and_then(|()| file.sync_all());
    if let Err(e) = written {
        let _ = std::fs::remove_file(path);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(recipient)
}

#[cfg(test)]
mod tests {
    use super::*;
    use age_core::format::{FileKey, Stanza, FILE_KEY_BYTES};
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;

    fn key() -> (age::x25519::Identity, String) {
        let identity = age::x25519::Identity::generate();
        let public = identity.to_public().to_string();
        (identity, public)
    }

    fn open(identity: &dyn age::Identity, armored: &str) -> anyhow::Result<Vec<u8>> {
        let decryptor = age::Decryptor::new_buffered(age::armor::ArmoredReader::new(armored.as_bytes()))?;
        let mut plain = decryptor.decrypt(std::iter::once(identity))?;
        let mut out = Vec::new();
        plain.read_to_end(&mut out)?;
        Ok(out)
    }

    fn identity_file(dir: &Path, identity: &age::x25519::Identity) -> std::path::PathBuf {
        let path = dir.join("owner.key");
        std::fs::write(&path, format!("# a comment\n\n{}\n", identity.to_string().expose_secret())).unwrap();
        path
    }

    /// Stands in for a key type this module has never heard of: its own stanza,
    /// behind nothing but the crate's traits. The "wrapping" is a XOR: a test.
    struct StandIn;
    struct StandInKey;
    const TAG: &str = "vault-stand-in";
    const PAD: u8 = 0x5a;

    impl age::Recipient for StandInKey {
        fn wrap_file_key(&self, file_key: &FileKey) -> Result<(Vec<Stanza>, HashSet<String>), age::EncryptError> {
            let body = file_key.expose_secret().iter().map(|b| b ^ PAD).collect();
            Ok((vec![Stanza { tag: TAG.into(), args: vec![], body }], HashSet::new()))
        }
    }

    impl age::Identity for StandInKey {
        fn unwrap_stanza(&self, stanza: &Stanza) -> Option<Result<FileKey, age::DecryptError>> {
            if stanza.tag != TAG {
                return None;
            }
            let mut key = [0u8; FILE_KEY_BYTES];
            for (k, b) in key.iter_mut().zip(&stanza.body) {
                *k = b ^ PAD;
            }
            Some(Ok(FileKey::init_with_mut(|file_key| *file_key = key)))
        }
    }

    impl KeyFormat for StandIn {
        fn recipient(&self, line: &str) -> Option<anyhow::Result<Box<dyn age::Recipient>>> {
            (line == "stand-in-recipient").then(|| Ok(Box::new(StandInKey) as Box<dyn age::Recipient>))
        }

        fn identity(&self, line: &str) -> Option<anyhow::Result<Box<dyn age::Identity>>> {
            (line == "stand-in-identity").then(|| Ok(Box::new(StandInKey) as Box<dyn age::Identity>))
        }
    }

    #[test]
    fn a_new_format_needs_no_change_to_encrypt_or_reseal() {
        let formats: &[&dyn KeyFormat] = &[&X25519, &StandIn];
        let dir = tempfile::tempdir().unwrap();
        let (software, public) = key();
        let recipients = parse_recipients(&format!("stand-in-recipient\n{public}\n"), formats).unwrap();
        let ciphertext = encrypt(&recipients, &mut &b"through the seam"[..]).unwrap();
        // Both recipients are in the file.
        assert_eq!(open(&software, &ciphertext).unwrap(), b"through the seam");
        let path = dir.path().join("handle");
        std::fs::write(&path, "stand-in-identity\n").unwrap();
        let (one_time, one_time_public) = key();
        let sealed = reseal(&path, formats, &ciphertext, &one_time_public).unwrap();
        assert_eq!(open(&one_time, &sealed).unwrap(), b"through the seam");
        // Without the format, the same lines are refused.
        assert!(parse_recipients("stand-in-recipient\n", FORMATS).is_err());
        assert!(load_identities(&path, FORMATS).is_err());
    }

    #[test]
    fn every_recipient_in_a_list_opens_it() {
        let keys: Vec<_> = (0..7).map(|_| key()).collect();
        let text: String = keys.iter().map(|(_, public)| format!("# someone\r\n  {public}  \r\n\r\n")).collect();
        let recipients = parse_recipients(&text, FORMATS).unwrap();
        assert_eq!(recipients.len(), 7);
        let ciphertext = encrypt(&recipients, &mut &b"to all of them"[..]).unwrap();
        for (identity, _) in &keys {
            assert_eq!(open(identity, &ciphertext).unwrap(), b"to all of them");
        }
        let (stranger, _) = key();
        assert!(open(&stranger, &ciphertext).is_err());
    }

    /// A list with a line left out would encrypt, and lose a key.
    #[test]
    fn a_line_no_format_reads_refuses_the_list() {
        let (_, public) = key();
        for line in ["age1se1qgg72x2qfk9wg3wh0qg9u0v7l5dkq4jx69fv80p6wdus4nkr3r3vkwnc", "ssh-ed25519 AAAA", "age1notakey", "nonsense"] {
            let error = parse_recipients(&format!("{public}\n{line}\n"), FORMATS).err().unwrap();
            assert!(error.to_string().contains("line 2"), "{error}");
        }
    }

    #[test]
    fn a_bad_identity_line_is_never_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        for line in ["AGE-SECRET-KEY-1THISISNOTAKEYBUTITCOULDBEONE", "AGE-PLUGIN-SE-1THISISAHANDLE"] {
            std::fs::write(&path, format!("{line}\n")).unwrap();
            let error = load_identities(&path, FORMATS).err().unwrap();
            let text = format!("{error:#}");
            assert!(!text.contains("THISIS"), "{text}");
            assert!(text.contains("line 1"), "{text}");
        }
        std::fs::write(&path, "# only a comment\n").unwrap();
        assert!(load_identities(&path, FORMATS).is_err());
    }

    /// Past age's 64 KiB chunk and past this module's buffer, byte for byte.
    #[test]
    fn a_large_value_survives_a_reseal_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, public) = key();
        let path = identity_file(dir.path(), &owner);
        let value: Vec<u8> = (0..300_001u32).map(|i| (i % 251) as u8).collect();
        let recipients = parse_recipients(&public, FORMATS).unwrap();
        let ciphertext = encrypt(&recipients, &mut value.as_slice()).unwrap();
        let (one_time, one_time_public) = key();
        let sealed = reseal(&path, FORMATS, &ciphertext, &one_time_public).unwrap();
        assert_eq!(open(&one_time, &sealed).unwrap(), value);
    }

    #[test]
    fn a_seal_is_to_the_one_time_key_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, public) = key();
        let path = identity_file(dir.path(), &owner);
        let recipients = parse_recipients(&public, FORMATS).unwrap();
        let ciphertext = encrypt(&recipients, &mut &b"value"[..]).unwrap();
        let (one_time, one_time_public) = key();
        let sealed = reseal(&path, FORMATS, &ciphertext, &one_time_public).unwrap();
        assert_eq!(open(&one_time, &sealed).unwrap(), b"value");
        assert!(open(&owner, &sealed).is_err());
    }

    /// `age -d | age -r` left a valid seal of whatever came out; this must not.
    #[test]
    fn a_truncated_ciphertext_gives_no_seal() {
        use age::armor::{ArmoredWriter, Format};
        let dir = tempfile::tempdir().unwrap();
        let (owner, public) = key();
        let path = identity_file(dir.path(), &owner);
        let value = vec![7u8; 200_000];
        let recipients = parse_recipients(&public, FORMATS).unwrap();
        let armored = encrypt(&recipients, &mut value.as_slice()).unwrap();
        // Cut in the middle of the payload: the header stays good.
        let mut binary = Vec::new();
        age::armor::ArmoredReader::new(armored.as_bytes()).read_to_end(&mut binary).unwrap();
        binary.truncate(binary.len() - 100_000);
        let mut writer = ArmoredWriter::wrap_output(Vec::new(), Format::AsciiArmor).unwrap();
        writer.write_all(&binary).unwrap();
        let cut = String::from_utf8(writer.finish().unwrap()).unwrap();
        let (_, one_time_public) = key();
        assert!(reseal(&path, FORMATS, &cut, &one_time_public).is_err());
    }

    #[test]
    fn a_wrong_identity_or_a_bad_one_time_key_gives_no_seal() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, public) = key();
        let path = identity_file(dir.path(), &owner);
        let recipients = parse_recipients(&public, FORMATS).unwrap();
        let ciphertext = encrypt(&recipients, &mut &b"value"[..]).unwrap();
        for bad in ["", "age1notakey", "-o/tmp/x"] {
            assert!(reseal(&path, FORMATS, &ciphertext, bad).is_err(), "{bad}");
        }
        let (wrong, _) = key();
        let wrong_path = dir.path().join("wrong.key");
        std::fs::write(&wrong_path, format!("{}\n", wrong.to_string().expose_secret())).unwrap();
        let (_, one_time_public) = key();
        assert!(reseal(&wrong_path, FORMATS, &ciphertext, &one_time_public).is_err());
    }

    /// What a mobile build calls: no stdout, no terminal, the bytes in hand.
    #[test]
    fn recover_returns_the_value_and_only_for_what_the_manifest_holds() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, public) = key();
        let path = identity_file(dir.path(), &owner);
        let manifest = dir.path().join("manifest.json");
        let recipients = parse_recipients(&public, FORMATS).unwrap();
        let value: Vec<u8> = (0..300_001u32).map(|i| (i % 251) as u8).collect();
        let good = encrypt(&recipients, &mut value.as_slice()).unwrap();
        let other = encrypt(&recipients, &mut &b"another"[..]).unwrap();
        crate::manifest::record(&manifest, "s", &good).unwrap();
        assert_eq!(*recover(&manifest, "s", &path, FORMATS, &good).unwrap(), value);
        assert!(recover(&manifest, "s", &path, FORMATS, &other).is_err(), "a swap");
        assert!(recover(&manifest, "t", &path, FORMATS, &good).is_err(), "no entry");
        assert!(recover(&dir.path().join("none.json"), "s", &path, FORMATS, &good).is_err(), "no manifest");
        // The check comes first: this is no identity at all, and says nothing.
        let error = recover(&manifest, "s", &dir.path().join("no.key"), FORMATS, &other).err().unwrap();
        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn a_recovery_identity_is_owner_only_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery.key");
        let recipient = generate_recovery_identity(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let before = std::fs::read(&path).unwrap();
        assert!(generate_recovery_identity(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let recipients = parse_recipients(&recipient, FORMATS).unwrap();
        let ciphertext = encrypt(&recipients, &mut &b"recovered"[..]).unwrap();
        let identities = load_identities(&path, FORMATS).unwrap();
        assert_eq!(open(identities[0].as_ref(), &ciphertext).unwrap(), b"recovered");
    }

    #[tokio::test]
    async fn the_deadline_gives_up_on_a_stuck_identity() {
        let (hold, wait) = std::sync::mpsc::channel::<()>();
        let start = std::time::Instant::now();
        let outcome = with_deadline(Duration::from_millis(50), move || {
            let _ = wait.recv();
            Ok(())
        })
        .await;
        assert!(outcome.is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(hold);
    }
}
