//! This crate's output against a real age CLI, both ways. Ignored by default:
//! the vault needs no CLI, and nothing fetches one. To run them:
//!
//!     VAULT_AGE_CLI_DIR=/dir/holding/age PLEXUS_NO_REGISTRY=1 \
//!       cargo test -p hyperforge-vault --test interop -- --ignored
//!
//! Throwaway keys only.

use hyperforge_vault::keys;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const APPROVE: &str = env!("CARGO_BIN_EXE_vault-approve");

fn cli(tool: &str) -> PathBuf {
    let dir = std::env::var("VAULT_AGE_CLI_DIR").expect("VAULT_AGE_CLI_DIR must name a directory holding age");
    let path = Path::new(&dir).join(tool);
    assert!(path.exists(), "{} is not there", path.display());
    path
}

/// Runs the CLI, `input` on stdin, and gives back stdout.
fn age(args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new(cli("age")).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success(), "age {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn ours(dir: &Path, stem: &str) -> (String, String) {
    let path = dir.join(stem);
    let out = Command::new(APPROVE).env_clear().arg("recovery-keygen").arg("--out").arg(&path).output().unwrap();
    assert!(out.status.success());
    (path.display().to_string(), String::from_utf8(out.stdout).unwrap().trim().to_string())
}

fn theirs(dir: &Path, stem: &str) -> (String, String) {
    let path = dir.join(stem);
    let out = Command::new(cli("age-keygen")).arg("-o").arg(&path).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stderr).unwrap();
    (path.display().to_string(), text.trim().strip_prefix("Public key: ").unwrap().to_string())
}

fn encrypted_by_us(value: &[u8], recipients: &str) -> String {
    let parsed = keys::parse_recipients(recipients, keys::FORMATS).unwrap();
    keys::encrypt(&parsed, &mut &value[..]).unwrap()
}

/// Past age's 64 KiB chunk, and not text.
fn value() -> Vec<u8> {
    (0..300_001u32).map(|i| (i % 251) as u8).collect()
}

fn open(identity: &str, ciphertext: &[u8]) -> Vec<u8> {
    let identities = keys::load_identities(Path::new(identity), keys::FORMATS).unwrap();
    let reader = age::armor::ArmoredReader::new(ciphertext);
    let decryptor = age::Decryptor::new_buffered(reader).unwrap();
    let mut plain = decryptor.decrypt(identities.iter().map(|i| i.as_ref())).unwrap();
    let mut out = Vec::new();
    plain.read_to_end(&mut out).unwrap();
    out
}

#[test]
#[ignore = "needs VAULT_AGE_CLI_DIR"]
fn crate_ciphertext_opens_with_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let (cli_key, cli_public) = theirs(dir.path(), "theirs.key");
    let (our_key, our_public) = ours(dir.path(), "ours.key");
    // Mixed: both keys, from both generators, each through the CLI.
    let armored = encrypted_by_us(&value(), &format!("# mixed\n{cli_public}\n{our_public}\n"));
    assert_eq!(age(&["-d", "-i", &cli_key], armored.as_bytes()), value());
    assert_eq!(age(&["-d", "-i", &our_key], armored.as_bytes()), value());
}

#[test]
#[ignore = "needs VAULT_AGE_CLI_DIR"]
fn cli_ciphertext_opens_with_the_crate() {
    let dir = tempfile::tempdir().unwrap();
    let (cli_key, cli_public) = theirs(dir.path(), "theirs.key");
    let (our_key, our_public) = ours(dir.path(), "ours.key");
    for flags in [&["-a"][..], &[][..]] {
        let mut args = flags.to_vec();
        args.extend(["-r", &cli_public, "-r", &our_public]);
        let ciphertext = age(&args, &value());
        assert_eq!(open(&cli_key, &ciphertext), value());
        assert_eq!(open(&our_key, &ciphertext), value());
    }
}

/// The release path: CLI-encrypted, resealed by us, opened by the CLI with the
/// one-time key alone.
#[test]
#[ignore = "needs VAULT_AGE_CLI_DIR"]
fn a_reseal_of_cli_ciphertext_opens_with_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let (owner, owner_public) = theirs(dir.path(), "owner.key");
    let (one_time, one_time_public) = theirs(dir.path(), "one-time.key");
    let ciphertext = String::from_utf8(age(&["-a", "-r", &owner_public], &value())).unwrap();
    let sealed = keys::reseal(Path::new(&owner), keys::FORMATS, &ciphertext, &one_time_public).unwrap();
    assert_eq!(age(&["-d", "-i", &one_time], sealed.as_bytes()), value());
    let refused = Command::new(cli("age")).args(["-d", "-i", &owner]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // It may refuse on the header and close its stdin before this is all written.
    let _ = refused.stdin.as_ref().unwrap().write_all(sealed.as_bytes());
    assert!(!refused.wait_with_output().unwrap().status.success());
}
