//! The Secure Enclave format, through `keys.rs`, over a software stand-in. What
//! only the real enclave can show is in ENROL.md's step 1 and the owner's to run.

#[path = "support/soft_enclave.rs"]
mod soft_enclave;

use hyperforge_vault::enclave::{GatedKey, Unattended};
use hyperforge_vault::keys::{self, KeyFormat, SecureEnclave, X25519};
use soft_enclave::{Access, SoftEnclave};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

fn formats(hardware: &'static SoftEnclave) -> Vec<Box<dyn KeyFormat>> {
    vec![Box::new(X25519), Box::new(SecureEnclave(hardware))]
}

fn refs(formats: &[Box<dyn KeyFormat>]) -> Vec<&dyn KeyFormat> {
    formats.iter().map(|f| f.as_ref()).collect()
}

struct Enrolled {
    _dir: tempfile::TempDir,
    identity: PathBuf,
    recipient: String,
    hardware: &'static SoftEnclave,
}

fn enrolled(hardware: &'static SoftEnclave) -> Enrolled {
    let dir = tempfile::tempdir().unwrap();
    let identity = dir.path().join("owner.identity");
    let recipient = keys::enrol(&identity, hardware).unwrap();
    Enrolled { _dir: dir, identity, recipient, hardware }
}

fn one_time() -> (age::x25519::Identity, String) {
    let identity = age::x25519::Identity::generate();
    let public = identity.to_public().to_string();
    (identity, public)
}

fn open(identity: &dyn age::Identity, armored: &str) -> Vec<u8> {
    use std::io::Read;
    let reader = age::armor::ArmoredReader::new(armored.as_bytes());
    let decryptor = age::Decryptor::new_buffered(reader).unwrap();
    let mut plain = decryptor.decrypt(std::iter::once(identity)).unwrap();
    let mut out = Vec::new();
    plain.read_to_end(&mut out).unwrap();
    out
}

/// A line for a key enrolment would never have written, as if placed by hand.
fn write_line(path: &Path, hardware: &'static SoftEnclave) {
    let hrp = bech32::Hrp::parse_unchecked("age-vault-enclave-");
    let line = age_core::primitives::bech32_encode(hrp, &hardware.blob()).to_uppercase();
    std::fs::write(path, format!("{line}\n")).unwrap();
}

#[test]
fn an_enclave_recipient_encrypts_and_touch_id_opens_it() {
    let key = enrolled(SoftEnclave::gated());
    let f = formats(key.hardware);
    let f = refs(&f);
    let recipients = keys::parse_recipients(&key.recipient, &f).unwrap();
    let armored = keys::encrypt(&recipients, &mut &b"through the enclave"[..]).unwrap();
    assert!(armored.contains("BEGIN AGE ENCRYPTED FILE"));
    let (one_time, public) = one_time();
    assert_eq!(key.hardware.prompts(), 0, "encrypting needs nobody");
    let sealed = keys::reseal(&key.identity, &f, &armored, &public).unwrap();
    assert_eq!(key.hardware.prompts(), 1, "one Touch ID, for one decrypt");
    assert_eq!(open(&one_time, &sealed), b"through the enclave");
}

/// The file as ENROL.md writes it: the enclave and the recovery key, side by side.
#[test]
fn a_recovery_recipient_sits_beside_the_enclave_and_each_opens_alone() {
    let key = enrolled(SoftEnclave::gated());
    let f = formats(key.hardware);
    let f = refs(&f);
    let (recovery, recovery_public) = one_time();
    let recipients = keys::parse_recipients(&format!("# owner\n{}\n# recovery\n{recovery_public}\n", key.recipient), &f).unwrap();
    assert_eq!(recipients.len(), 2);
    let armored = keys::encrypt(&recipients, &mut &b"either key"[..]).unwrap();
    assert_eq!(open(&recovery, &armored), b"either key");
    assert_eq!(key.hardware.prompts(), 0, "the recovery key fired no Touch ID");
    let (one_time, public) = one_time();
    assert_eq!(open(&one_time, &keys::reseal(&key.identity, &f, &armored, &public).unwrap()), b"either key");
}

#[test]
fn a_secret_for_someone_else_never_fires_touch_id() {
    let (mine, theirs) = (enrolled(SoftEnclave::gated()), enrolled(SoftEnclave::gated()));
    let f = formats(mine.hardware);
    let f = refs(&f);
    let recipients = keys::parse_recipients(&theirs.recipient, &f).unwrap();
    let armored = keys::encrypt(&recipients, &mut &b"not yours"[..]).unwrap();
    let (_, public) = one_time();
    assert!(keys::reseal(&mine.identity, &f, &armored, &public).is_err());
    assert_eq!(mine.hardware.prompts(), 0);
}

#[test]
fn a_cancelled_touch_id_gives_no_seal() {
    let key = enrolled(SoftEnclave::new(Access::CurrentBiometry, Unattended::WantsTheOwner, Some(-2)));
    let f = formats(key.hardware);
    let f = refs(&f);
    let recipients = keys::parse_recipients(&key.recipient, &f).unwrap();
    let armored = keys::encrypt(&recipients, &mut &b"cancelled"[..]).unwrap();
    let (_, public) = one_time();
    assert!(keys::reseal(&key.identity, &f, &armored, &public).is_err());
    assert_eq!(key.hardware.prompts(), 1);
}

/// `GatedKey` and the format are the only way from a line to the hardware.
#[test]
fn a_key_with_no_gate_is_refused_before_touch_id() {
    let hardware = SoftEnclave::new(Access::Nothing, Unattended::Opened, None);
    assert!(!hyperforge_vault::enclave::requires_current_biometry(&hardware.blob()));
    let error = GatedKey::open(hardware, Zeroizing::new(hardware.blob())).err().unwrap();
    assert!(error.to_string().contains("refused"), "{error}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ungated.identity");
    write_line(&path, hardware);
    let f = formats(hardware);
    let error = keys::load_identities(&path, &refs(&f)).err().unwrap();
    assert!(format!("{error:#}").contains("refused"), "{error:#}");
    assert_eq!(hardware.prompts(), 0);
}

/// The blob says `cbio`, and the enclave opens it with nobody there: the blob lies,
/// and the enclave is believed.
#[test]
fn a_key_the_enclave_opens_unattended_is_refused_whatever_it_claims() {
    let hardware = SoftEnclave::new(Access::CurrentBiometry, Unattended::Opened, None);
    let error = GatedKey::open(hardware, Zeroizing::new(hardware.blob())).err().unwrap();
    assert!(error.to_string().contains("without Touch ID"), "{error}");
}

/// A damaged blob, and an ungated key with `cbio` spliced in, both load and both
/// read as gated. Only the enclave's refusal tells them from a real one: on macOS 14
/// it says `CryptoTokenKit -3` to both, where a real one says it needs the owner.
#[test]
fn a_key_the_enclave_cannot_use_is_refused_however_it_reads() {
    let hardware = SoftEnclave::new(Access::CurrentBiometry, Unattended::Failed(-3), None);
    assert!(hyperforge_vault::enclave::requires_current_biometry(&hardware.blob()));
    let error = GatedKey::open(hardware, Zeroizing::new(hardware.blob())).err().unwrap();
    assert!(error.to_string().contains("cannot use this key"), "{error}");
    assert_eq!(hardware.prompts(), 0);
}

#[test]
fn enrolment_refuses_a_key_that_came_back_ungated_and_leaves_no_file() {
    for hardware in [SoftEnclave::new(Access::Nothing, Unattended::WantsTheOwner, None), SoftEnclave::new(Access::CurrentBiometry, Unattended::Opened, None), SoftEnclave::new(Access::CurrentBiometry, Unattended::Failed(-3), None)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner.identity");
        assert!(keys::enrol(&path, hardware).is_err());
        assert!(!path.exists());
    }
}

#[test]
fn enrolment_is_owner_only_never_overwrites_and_checks_out() {
    let key = enrolled(SoftEnclave::gated());
    assert_eq!(std::fs::metadata(&key.identity).unwrap().permissions().mode() & 0o777, 0o600);
    let before = std::fs::read(&key.identity).unwrap();
    assert!(keys::enrol(&key.identity, key.hardware).is_err());
    assert_eq!(std::fs::read(&key.identity).unwrap(), before);
    assert!(std::str::from_utf8(&before).unwrap().lines().any(|l| l.starts_with("AGE-VAULT-ENCLAVE-1")));
    assert_eq!(keys::check_enrolment(&key.identity, key.hardware).unwrap(), key.recipient);
    assert_eq!(key.hardware.prompts(), 0, "neither enrolling nor checking fires Touch ID");
}

#[test]
fn a_malformed_enclave_line_is_refused_and_never_quoted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key");
    let hardware = SoftEnclave::gated();
    let f = formats(hardware);
    std::fs::write(&path, "AGE-VAULT-ENCLAVE-1THISISNOTAKEY\n").unwrap();
    let error = keys::load_identities(&path, &refs(&f)).err().unwrap();
    let text = format!("{error:#}");
    assert!(text.contains("line 1") && !text.contains("THISIS"), "{text}");
    assert!(keys::parse_recipients("age1tag1notarecipient", &refs(&f)).is_err());
    // A flipped letter fails the checksum.
    let key = enrolled(hardware);
    let good = std::fs::read_to_string(&key.identity).unwrap();
    let line = good.lines().find(|l| l.starts_with("AGE-VAULT")).unwrap();
    let flipped = format!("{}{}", &line[..line.len() - 1], if line.ends_with('Q') { 'P' } else { 'Q' });
    std::fs::write(&path, flipped).unwrap();
    assert!(keys::load_identities(&path, &refs(&f)).is_err());
}

/// The build as it ships, on a machine with no enclave: the recipient still
/// encrypts, and the identity is refused out loud.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_build_with_no_enclave_refuses_and_says_so() {
    let key = enrolled(SoftEnclave::gated());
    assert_eq!(keys::parse_recipients(&key.recipient, keys::FORMATS).unwrap().len(), 1);
    let error = keys::load_identities(&key.identity, keys::FORMATS).err().unwrap();
    assert!(format!("{error:#}").contains("no Secure Enclave"), "{error:#}");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owner.identity");
    assert!(keys::enrol(&path, &hyperforge_vault::enclave::ThisMachine).is_err());
    assert!(!path.exists());
}

fn shipped_source() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![root.join("build.rs"), root.join("shim/seshim.swift")];
    for dir in ["src", "src/bin"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "rs") {
                paths.push(path);
            }
        }
    }
    paths.into_iter().map(|p| { let text = std::fs::read_to_string(&p).unwrap(); (p, text) }).collect()
}

/// What the spike's review said must never ship, held by a test and not a memory.
#[test]
fn nothing_shipped_stores_exports_or_weakens_a_key() {
    let forbidden = ["kSecUseDataProtectionKeychain", "SecItem", "kSecAttrIsPermanent", "SecKeyCopyExternalRepresentation", "SecKeyCreateRandomKey", "kSecAttrTokenID", "toid", "biometryAny", "userPresence", ".devicePasscode", "applicationPassword"];
    for (path, text) in shipped_source() {
        for word in forbidden {
            assert!(!text.contains(word), "{} says {word}", path.display());
        }
    }
}

#[test]
fn the_shim_can_only_make_a_key_with_the_gate() {
    let shim = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/shim/seshim.swift")).unwrap();
    assert_eq!(shim.matches("SecAccessControlCreateWithFlags").count(), 1);
    assert_eq!(shim.matches("[.privateKeyUsage, .biometryCurrentSet]").count(), 1);
    assert_eq!(shim.matches("PrivateKey(accessControl:").count(), 1);
    assert!(!shim.contains("mode"), "the spike's modes: forbidding the interface is not a caller's choice");
}

/// The core is what a mobile build shares: the key's holder is a trait, full stop.
#[test]
fn the_core_knows_no_process_no_path_and_no_platform() {
    let core = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/p256tag.rs")).unwrap();
    for word in ["std::fs", "std::process", "std::path", "std::env", "Command", "extern \"C\"", "target_os", "target_vendor", "enclave::"] {
        assert!(!core.contains(word), "p256tag.rs says {word}");
    }
}
