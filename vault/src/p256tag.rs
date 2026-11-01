//! age's `p256tag` stanza, decrypting. Portable and pure: a key lives behind
//! [`KeyAgreement`], and that trait is the whole boundary, so nothing here knows
//! what holds it. Encrypting is `age::tag::Recipient`, and has no code here.
//!
//! This is RFC 9180 base-mode HPKE taken apart at the one step a hardware key does
//! for us, the Diffie-Hellman: DHKEM(P-256, HKDF-SHA256), HKDF-SHA256,
//! ChaCha20Poly1305. The `hpke` crate cannot do that: it wants the private scalar.

use age::{DecryptError, Identity};
use age_core::format::{FileKey, Stanza};
use age_core::primitives::bech32_encode;
use base64::{prelude::BASE64_STANDARD_NO_PAD, Engine};
use bech32::Hrp;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use p256::{elliptic_curve::sec1::ToEncodedPoint, PublicKey};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

const STANZA_TAG: &str = "p256tag";
const RECIPIENT_PREFIX: Hrp = Hrp::parse_unchecked("age1tag");
/// Both the salt of the stanza's tag and the HPKE info string.
const SALT: &[u8] = b"age-encryption.org/p256tag";
const KEM_SUITE: &[u8] = b"KEM\x00\x10";
const HPKE_SUITE: &[u8] = b"HPKE\x00\x10\x00\x01\x00\x03";
/// A file key and its Poly1305 tag.
const BODY_LENGTH: usize = 32;

/// What a key holder does. Points are X9.63 uncompressed, the secret 32 bytes.
pub trait KeyAgreement {
    fn public_key(&self) -> [u8; 65];
    /// May block on the owner. `Err` is a platform code, for diagnostics only.
    fn ecdh(&self, peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32>;
}

fn labeled_extract(suite: &[u8], salt: &[u8], label: &[u8], ikm: &[u8]) -> Zeroizing<[u8; 32]> {
    let ikm = Zeroizing::new([b"HPKE-v1", suite, label, ikm].concat());
    let (mut prk, _) = Hkdf::<Sha256>::extract(Some(salt), &ikm);
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&prk);
    prk.as_mut_slice().zeroize();
    out
}

fn labeled_expand<const L: usize>(suite: &[u8], prk: &[u8; 32], label: &[u8], info: &[u8]) -> Zeroizing<[u8; L]>
where
    [u8; L]: Zeroize,
{
    let full = [&(L as u16).to_be_bytes()[..], b"HPKE-v1", suite, label, info].concat();
    let mut out = Zeroizing::new([0u8; L]);
    Hkdf::<Sha256>::from_prk(prk).expect("a 32-byte PRK").expand(&full, &mut out[..]).expect("a short output");
    out
}

/// RFC 9180's Decap and KeySchedule, after the DH: the shared secret, key and nonce.
fn hpke_schedule(z: &[u8; 32], enc: &[u8; 65], pk_r: &[u8; 65], info: &[u8]) -> (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>, Zeroizing<[u8; 12]>) {
    let eae = labeled_extract(KEM_SUITE, b"", b"eae_prk", z);
    let shared = labeled_expand::<32>(KEM_SUITE, &eae, b"shared_secret", &[&enc[..], &pk_r[..]].concat());
    let psk_hash = labeled_extract(HPKE_SUITE, b"", b"psk_id_hash", b"");
    let info_hash = labeled_extract(HPKE_SUITE, b"", b"info_hash", info);
    let context = [&[0u8][..], &psk_hash[..], &info_hash[..]].concat();
    let secret = labeled_extract(HPKE_SUITE, &shared[..], b"secret", b"");
    let key = labeled_expand::<32>(HPKE_SUITE, &secret, b"key", &context);
    let nonce = labeled_expand::<12>(HPKE_SUITE, &secret, b"base_nonce", &context);
    (shared, key, nonce)
}

fn compressed(pk: &[u8; 65]) -> Option<Vec<u8>> {
    Some(PublicKey::from_sec1_bytes(pk).ok()?.to_encoded_point(true).as_bytes().to_vec())
}

fn stanza_tag(enc: &[u8; 65], pk: &[u8; 65]) -> Option<[u8; 4]> {
    let static_tag = &Sha256::digest(compressed(pk)?)[..4];
    let (tag, _) = Hkdf::<Sha256>::extract(Some(SALT), &[&enc[..], static_tag].concat());
    Some(tag[..4].try_into().expect("four bytes"))
}

/// `age1tag1…` for a public key, or `None` if it is not a point on the curve.
pub fn recipient_string(pk: &[u8; 65]) -> Option<String> {
    Some(bech32_encode(RECIPIENT_PREFIX, &compressed(pk)?))
}

/// An age identity over a [`KeyAgreement`].
pub struct P256TagIdentity<K> {
    key: K,
}

impl<K: KeyAgreement> P256TagIdentity<K> {
    pub fn new(key: K) -> Self {
        Self { key }
    }
}

impl<K: KeyAgreement> Identity for P256TagIdentity<K> {
    fn unwrap_stanza(&self, _: &Stanza) -> Option<Result<FileKey, DecryptError>> {
        None // red: not written yet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::Recipient as _;
    use p256::{ecdh::diffie_hellman, SecretKey};
    use std::cell::Cell;
    use std::rc::Rc;

    struct Soft {
        secret: SecretKey,
        calls: Rc<Cell<u32>>,
        refuse: Option<i32>,
    }

    impl Soft {
        fn new() -> Self {
            Soft { secret: SecretKey::random(&mut rand_core::OsRng), calls: Rc::default(), refuse: None }
        }

        fn recipient(&self) -> age::tag::Recipient {
            recipient_string(&self.public_key()).unwrap().parse().unwrap()
        }
    }

    impl KeyAgreement for Soft {
        fn public_key(&self) -> [u8; 65] {
            self.secret.public_key().to_encoded_point(false).as_bytes().try_into().unwrap()
        }

        fn ecdh(&self, peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32> {
            self.calls.set(self.calls.get() + 1);
            if let Some(code) = self.refuse {
                return Err(code);
            }
            let peer = PublicKey::from_sec1_bytes(peer).unwrap();
            let shared = diffie_hellman(self.secret.to_nonzero_scalar(), peer.as_affine());
            let mut out = Zeroizing::new([0u8; 32]);
            out.copy_from_slice(shared.raw_secret_bytes().as_ref());
            Ok(out)
        }
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    fn wrapped(key: &Soft) -> (FileKey, Stanza) {
        let file_key = FileKey::init_with_mut(|bytes| bytes.fill(7));
        let (mut stanzas, _) = key.recipient().wrap_file_key(&file_key).unwrap();
        (file_key, stanzas.remove(0))
    }

    /// RFC 9180 A.5.1 and A.5.1.1, sequence 0: the suite p256tag uses.
    #[test]
    fn the_schedule_gives_rfc_9180s_vectors() {
        let secret = SecretKey::from_slice(&hex("a4d1c55836aa30f9b3fbb6ac98d338c877c2867dd3a77396d13f68d3ab150d3b")).unwrap();
        let key = Soft { secret, calls: Rc::default(), refuse: None };
        let enc: [u8; 65] = hex("04c07836a0206e04e31d8ae99bfd549380b072a1b1b82e563c935c095827824fc1559eac6fb9e3c70cd3193968994e7fe9781aa103f5b50e934b5b2f387e381291").try_into().unwrap();
        assert_eq!(key.public_key()[..], hex("04a697bffde9405c992883c5c439d6cc358170b51af72812333b015621dc0f40bad9bb726f68a5c013806a790ec716ab8669f84f6b694596c2987cf35baba2a006")[..]);
        let z = key.ecdh(&enc).unwrap();
        let (shared, aead_key, nonce) = hpke_schedule(&z, &enc, &key.public_key(), &hex("4f6465206f6e2061204772656369616e2055726e"));
        assert_eq!(shared[..], hex("806520f82ef0b03c823b7fc524b6b55a088f566b9751b89551c170f4113bd850")[..]);
        assert_eq!(aead_key[..], hex("a8f45490a92a3b04d1dbf6cf2c3939ad8bfc9bfcb97c04bffe116730c9dfe3fc")[..]);
        assert_eq!(nonce[..], hex("726b4390ed2209809f58c693")[..]);
        let ciphertext = hex("6469c41c5c81d3aa85432531ecf6460ec945bde1eb428cb2fedf7a29f5a685b4ccb0d057f03ea2952a27bb458b");
        let aad = hex("436f756e742d30");
        let plain = ChaCha20Poly1305::new(Key::from_slice(&aead_key[..])).decrypt(Nonce::from_slice(&nonce[..]), Payload { msg: &ciphertext, aad: &aad }).unwrap();
        assert_eq!(plain, hex("4265617574792069732074727574682c20747275746820626561757479"));
    }

    #[test]
    fn a_recipient_string_is_stock_ages() {
        let key = Soft::new();
        let text = recipient_string(&key.public_key()).unwrap();
        assert!(text.starts_with("age1tag1"), "{text}");
        assert_eq!(text.parse::<age::tag::Recipient>().unwrap().to_string(), text);
    }

    #[test]
    fn a_point_off_the_curve_has_no_recipient() {
        assert_eq!(recipient_string(&[4u8; 65]), None);
    }

    #[test]
    fn what_stock_age_wraps_this_unwraps_with_one_ecdh() {
        let key = Soft::new();
        let calls = key.calls.clone();
        let (file_key, stanza) = wrapped(&key);
        let opened = P256TagIdentity::new(key).unwrap_stanza(&stanza).unwrap().unwrap();
        use age::secrecy::ExposeSecret;
        assert_eq!(opened.expose_secret(), file_key.expose_secret());
        assert_eq!(calls.get(), 1);
    }

    /// Touch ID fires for nobody else's stanza.
    #[test]
    fn someone_elses_stanza_never_reaches_the_key() {
        let (theirs, ours) = (Soft::new(), Soft::new());
        let calls = ours.calls.clone();
        let (_, stanza) = wrapped(&theirs);
        assert!(P256TagIdentity::new(ours).unwrap_stanza(&stanza).is_none());
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn another_stanza_type_is_not_ours() {
        let key = Soft::new();
        let calls = key.calls.clone();
        let stanza = Stanza { tag: "X25519".into(), args: vec!["AAAA".into()], body: vec![0; 32] };
        assert!(P256TagIdentity::new(key).unwrap_stanza(&stanza).is_none());
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn a_refused_ecdh_is_a_failure_and_no_file_key() {
        let mut key = Soft::new();
        key.refuse = Some(-2);
        let (_, stanza) = wrapped(&key);
        assert!(matches!(P256TagIdentity::new(key).unwrap_stanza(&stanza), Some(Err(DecryptError::DecryptionFailed))));
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        let key = Soft::new();
        let (_, mut stanza) = wrapped(&key);
        stanza.body[0] ^= 1;
        assert!(matches!(P256TagIdentity::new(key).unwrap_stanza(&stanza), Some(Err(_))));
    }

    #[test]
    fn a_malformed_stanza_does_not_reach_the_key() {
        let key = Soft::new();
        let calls = key.calls.clone();
        let (_, good) = wrapped(&key);
        let identity = P256TagIdentity::new(key);
        let bad = [
            Stanza { tag: STANZA_TAG.into(), args: vec!["AAAAAA".into()], body: vec![0; 32] },
            Stanza { tag: STANZA_TAG.into(), args: vec![good.args[0].clone(), "!!".into()], body: vec![0; 32] },
            Stanza { tag: STANZA_TAG.into(), args: vec![good.args[0].clone(), good.args[1].clone(), "x".into()], body: vec![0; 32] },
            Stanza { tag: STANZA_TAG.into(), args: good.args.clone(), body: vec![0; 31] },
        ];
        for stanza in &bad {
            assert!(matches!(identity.unwrap_stanza(stanza), Some(Err(DecryptError::InvalidHeader))), "{:?}", stanza.args);
        }
        assert_eq!(calls.get(), 0);
    }

    /// The tag matches, as an attacker who knows the recipient can arrange. The
    /// point is junk: it goes no further than the check.
    #[test]
    fn a_point_off_the_curve_does_not_reach_the_key() {
        let key = Soft::new();
        let calls = key.calls.clone();
        let enc = [4u8; 65];
        let tag = stanza_tag(&enc, &key.public_key()).unwrap();
        let args = vec![BASE64_STANDARD_NO_PAD.encode(tag), BASE64_STANDARD_NO_PAD.encode(enc)];
        let stanza = Stanza { tag: STANZA_TAG.into(), args, body: vec![0; 32] };
        assert!(matches!(P256TagIdentity::new(key).unwrap_stanza(&stanza), Some(Err(DecryptError::InvalidHeader))));
        assert_eq!(calls.get(), 0);
    }
}
