//! Portable `p256tag` decryption for age. Pure Rust: no files, processes, or platform
//! APIs. A hardware key plugs in through `KeyAgreement`; that trait is the whole boundary.
use age::{DecryptError, Identity};
use age_core::format::{FileKey, Stanza};
use base64::{prelude::BASE64_STANDARD_NO_PAD, Engine};
use chacha20poly1305::{aead::{Aead, KeyInit, Payload}, ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use p256::{elliptic_curve::sec1::ToEncodedPoint, PublicKey};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::io::{Read, Write};
use zeroize::{Zeroize, Zeroizing};

pub const STANZA_TAG: &str = "p256tag";
const SALT: &[u8] = b"age-encryption.org/p256tag";
const KEM_SUITE: &[u8] = b"KEM\x00\x10";
const HPKE_SUITE: &[u8] = b"HPKE\x00\x10\x00\x01\x00\x03";

/// The narrow callback a key holder implements. X9.63 uncompressed points, 32-byte Z.
pub trait KeyAgreement {
    fn public_key(&self) -> [u8; 65];
    /// May block on user presence. `Err` carries a platform code for diagnostics only.
    fn ecdh(&self, peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32>;
}

fn labeled_extract(suite: &[u8], salt: &[u8], label: &[u8], ikm: &[u8]) -> Zeroizing<[u8; 32]> {
    let ikm = Zeroizing::new([b"HPKE-v1", suite, label, ikm].concat());
    let (prk, _) = Hkdf::<Sha256>::extract(Some(salt), &ikm);
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&prk);
    out
}
fn labeled_expand<const L: usize>(suite: &[u8], prk: &[u8; 32], label: &[u8], info: &[u8]) -> Zeroizing<[u8; L]> {
    let full = [&(L as u16).to_be_bytes()[..], b"HPKE-v1", suite, label, info].concat();
    let mut out = Zeroizing::new([0u8; L]);
    Hkdf::<Sha256>::from_prk(prk).expect("prk length").expand(&full, &mut out[..]).expect("okm length");
    out
}
/// RFC 9180 base-mode Decap + KeySchedule. Returns (shared_secret, key, base_nonce).
fn hpke_schedule(z: &[u8; 32], enc: &[u8; 65], pk_r: &[u8; 65], info: &[u8])
    -> (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>, Zeroizing<[u8; 12]>) {
    let eae = labeled_extract(KEM_SUITE, b"", b"eae_prk", z);
    let ss = labeled_expand::<32>(KEM_SUITE, &eae, b"shared_secret", &[&enc[..], &pk_r[..]].concat());
    let psk_h = labeled_extract(HPKE_SUITE, b"", b"psk_id_hash", b"");
    let info_h = labeled_extract(HPKE_SUITE, b"", b"info_hash", info);
    let ctx = [&[0u8][..], &psk_h[..], &info_h[..]].concat();
    let secret = labeled_extract(HPKE_SUITE, &ss[..], b"secret", b"");
    let key = labeled_expand::<32>(HPKE_SUITE, &secret, b"key", &ctx);
    let nonce = labeled_expand::<12>(HPKE_SUITE, &secret, b"base_nonce", &ctx);
    (ss, key, nonce)
}
fn compressed(pk: &[u8; 65]) -> Option<Vec<u8>> {
    Some(PublicKey::from_sec1_bytes(pk).ok()?.to_encoded_point(true).as_bytes().to_vec())
}
fn stanza_tag(enc: &[u8; 65], pk: &[u8; 65]) -> Option<[u8; 4]> {
    let st = &Sha256::digest(compressed(pk)?)[..4];
    let (t, _) = Hkdf::<Sha256>::extract(Some(SALT), &[&enc[..], st].concat());
    Some(t[..4].try_into().expect("len"))
}
/// `age1tag1...` for a key holder's public key.
pub fn recipient_string(pk: &[u8; 65]) -> Option<String> {
    struct W(Vec<u8>);
    impl std::fmt::Display for W {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            age_core::primitives::bech32_encode_to_fmt(f, bech32::Hrp::parse_unchecked("age1tag"), &self.0)
        }
    }
    Some(W(compressed(pk)?).to_string())
}

pub struct P256TagIdentity<K> { key: K, last_error: Cell<Option<i32>> }
impl<K: KeyAgreement> P256TagIdentity<K> {
    pub fn new(key: K) -> Self { Self { key, last_error: Cell::new(None) } }
    /// `DecryptError` cannot carry a platform code, so the cause of a refusal lands here.
    pub fn last_error(&self) -> Option<i32> { self.last_error.get() }
}
impl<K: KeyAgreement> Identity for P256TagIdentity<K> {
    fn unwrap_stanza(&self, s: &Stanza) -> Option<Result<FileKey, DecryptError>> {
        if s.tag != STANZA_TAG { return None; }
        let bad = || Some(Err(DecryptError::InvalidHeader));
        let [tag, enc] = &s.args[..] else { return bad() };
        let (Ok(tag), Ok(enc)) = (BASE64_STANDARD_NO_PAD.decode(tag), BASE64_STANDARD_NO_PAD.decode(enc)) else { return bad() };
        let (Ok(tag), Ok(enc)) = (<[u8; 4]>::try_from(tag), <[u8; 65]>::try_from(enc)) else { return bad() };
        if s.body.len() != 32 { return bad(); }
        let pk = self.key.public_key();
        // Must stay before ecdh(): a stanza for someone else must never prompt the user.
        if stanza_tag(&enc, &pk)? != tag { return None; }
        if PublicKey::from_sec1_bytes(&enc).is_err() { return bad(); }
        let z = match self.key.ecdh(&enc) {
            Ok(z) => z,
            Err(code) => { self.last_error.set(Some(code)); return Some(Err(DecryptError::DecryptionFailed)); }
        };
        let (_ss, key, nonce) = hpke_schedule(&z, &enc, &pk, SALT);
        let aead = ChaCha20Poly1305::new(Key::from_slice(&key[..]));
        let Ok(mut pt) = aead.decrypt(Nonce::from_slice(&nonce[..]), Payload { msg: &s.body, aad: b"" }) else {
            return Some(Err(DecryptError::DecryptionFailed));
        };
        let fk = FileKey::init_with_mut(|k| k.copy_from_slice(&pt));
        pt.zeroize();
        Some(Ok(fk))
    }
}
/// In-memory seal with the stock `age` recipient; no custom encrypt code exists.
pub fn seal(recipient: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let r: age::tag::Recipient = recipient.parse().map_err(|e: &str| e.to_string())?;
    let enc = age::Encryptor::with_recipients(std::iter::once(&r as &dyn age::Recipient)).map_err(|e| e.to_string())?;
    let mut out = vec![];
    let mut w = enc.wrap_output(&mut out).map_err(|e| e.to_string())?;
    w.write_all(plaintext).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(out)
}
pub fn open(ciphertext: &[u8], identity: &dyn Identity) -> Result<Vec<u8>, DecryptError> {
    let d = age::Decryptor::new(ciphertext)?;
    let mut r = d.decrypt(std::iter::once(identity))?;
    let mut out = vec![];
    r.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::{ecdh::diffie_hellman, SecretKey};
    use std::rc::Rc;
    struct Soft { sk: SecretKey, calls: Rc<Cell<u32>>, refuse: Option<i32> }
    impl Soft { fn new() -> Self { Self { sk: SecretKey::random(&mut rand_core::OsRng), calls: Rc::default(), refuse: None } } }
    fn x963(sk: &SecretKey) -> [u8; 65] { sk.public_key().to_encoded_point(false).as_bytes().try_into().unwrap() }
    impl KeyAgreement for Soft {
        fn public_key(&self) -> [u8; 65] { x963(&self.sk) }
        fn ecdh(&self, peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32> {
            self.calls.set(self.calls.get() + 1);
            if let Some(c) = self.refuse { return Err(c); }
            let p = PublicKey::from_sec1_bytes(peer).unwrap();
            let z = diffie_hellman(self.sk.to_nonzero_scalar(), p.as_affine());
            let mut o = Zeroizing::new([0u8; 32]); o.copy_from_slice(z.raw_secret_bytes().as_ref()); Ok(o)
        }
    }
    fn h(s: &str) -> Vec<u8> { hex::decode(s).unwrap() }
    fn a65(s: &str) -> [u8; 65] { h(s).try_into().unwrap() }

    /// RFC 9180 A.5.1 + A.5.1.1, sequence 0: the suite p256tag uses.
    #[test] fn rfc9180_a5_1() {
        let sk = SecretKey::from_slice(&h("a4d1c55836aa30f9b3fbb6ac98d338c877c2867dd3a77396d13f68d3ab150d3b")).unwrap();
        let k = Soft { sk, calls: Rc::default(), refuse: None };
        let enc = a65("04c07836a0206e04e31d8ae99bfd549380b072a1b1b82e563c935c095827824fc1559eac6fb9e3c70cd3193968994e7fe9781aa103f5b50e934b5b2f387e381291");
        assert_eq!(k.public_key()[..], h("04a697bffde9405c992883c5c439d6cc358170b51af72812333b015621dc0f40bad9bb726f68a5c013806a790ec716ab8669f84f6b694596c2987cf35baba2a006")[..]);
        let z = k.ecdh(&enc).unwrap();
        let (ss, key, nonce) = hpke_schedule(&z, &enc, &k.public_key(), &h("4f6465206f6e2061204772656369616e2055726e"));
        assert_eq!(ss[..], h("806520f82ef0b03c823b7fc524b6b55a088f566b9751b89551c170f4113bd850")[..]);
        assert_eq!(key[..], h("a8f45490a92a3b04d1dbf6cf2c3939ad8bfc9bfcb97c04bffe116730c9dfe3fc")[..]);
        assert_eq!(nonce[..], h("726b4390ed2209809f58c693")[..]);
        let ct = h("6469c41c5c81d3aa85432531ecf6460ec945bde1eb428cb2fedf7a29f5a685b4ccb0d057f03ea2952a27bb458b");
        let pt = ChaCha20Poly1305::new(Key::from_slice(&key[..])).decrypt(Nonce::from_slice(&nonce[..]), Payload { msg: &ct, aad: &h("436f756e742d30") }).unwrap();
        assert_eq!(pt, h("4265617574792069732074727574682c20747275746820626561757479"));
    }
    #[test] fn recipient_string_parses_in_stock_age() {
        let k = Soft::new(); let r = recipient_string(&k.public_key()).unwrap();
        let p: age::tag::Recipient = r.parse().unwrap(); assert_eq!(p.to_string(), r);
    }
    #[test] fn stock_age_output_round_trips() {
        let k = Soft::new(); let c = k.calls.clone();
        let ct = seal(&recipient_string(&k.public_key()).unwrap(), b"hello").unwrap();
        assert!(ct.windows(10).any(|w| w == b"-> p256tag"));
        assert_eq!(open(&ct, &P256TagIdentity::new(k)).unwrap(), b"hello");
        assert_eq!(c.get(), 1);
    }
    #[test] fn wrong_recipient_never_calls_ecdh() {
        let (a, b) = (Soft::new(), Soft::new()); let c = b.calls.clone();
        let ct = seal(&recipient_string(&a.public_key()).unwrap(), b"x").unwrap();
        assert!(matches!(open(&ct, &P256TagIdentity::new(b)), Err(DecryptError::NoMatchingKeys)));
        assert_eq!(c.get(), 0);
    }
    #[test] fn refused_ecdh_fails_and_keeps_code() {
        let mut k = Soft::new(); k.refuse = Some(-2);
        let ct = seal(&recipient_string(&k.public_key()).unwrap(), b"x").unwrap();
        let id = P256TagIdentity::new(k);
        assert!(open(&ct, &id).is_err()); assert_eq!(id.last_error(), Some(-2));
    }
    #[test] fn tampered_body_is_rejected() {
        let k = Soft::new();
        let id = P256TagIdentity::new(k);
        let r = recipient_string(&id.key.public_key()).unwrap();
        let mut body = None;
        let rr: age::tag::Recipient = r.parse().unwrap();
        let fk = FileKey::init_with_mut(|b| b.fill(7));
        if let Ok((st, _)) = age::Recipient::wrap_file_key(&rr, &fk) { body = Some(st); }
        let mut st = body.unwrap(); st[0].body[0] ^= 1;
        assert!(matches!(id.unwrap_stanza(&st[0]), Some(Err(_))));
    }
    #[test] fn malformed_args_do_not_reach_ecdh() {
        let k = Soft::new(); let c = k.calls.clone();
        let id = P256TagIdentity::new(k);
        let s = Stanza { tag: STANZA_TAG.into(), args: vec!["AAAAAA".into()], body: vec![0; 32] };
        assert!(matches!(id.unwrap_stanza(&s), Some(Err(DecryptError::InvalidHeader))));
        assert_eq!(c.get(), 0);
    }
}
