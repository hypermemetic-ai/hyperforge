//! A software stand-in for the Secure Enclave, for tests. Its key is in the blob,
//! in the clear: nothing about it may reach a binary, and nothing can, as it lives
//! here. The blob has the shape of a real one, so the real gate reads it.

use hyperforge_vault::enclave::{Hardware, Unattended};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use std::sync::atomic::{AtomicUsize, Ordering};
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq)]
pub enum Access {
    CurrentBiometry,
    Nothing,
}

pub struct SoftEnclave {
    pub access: Access,
    /// What the enclave would actually do with the interface forbidden.
    pub unattended: Unattended,
    /// Touch ID, cancelled with this code.
    pub refuse: Option<i32>,
    pub prompts: AtomicUsize,
}

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
    tlv(0x30, &[tlv(0x0c, name.as_bytes()), value].concat())
}

fn set(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x31, &parts.concat())
}

impl SoftEnclave {
    /// Leaked: `FORMATS` holds its hardware for the life of the process.
    pub fn new(access: Access, unattended: Unattended, refuse: Option<i32>) -> &'static SoftEnclave {
        Box::leak(Box::new(SoftEnclave { access, unattended, refuse, prompts: AtomicUsize::new(0) }))
    }

    pub fn gated() -> &'static SoftEnclave {
        Self::new(Access::CurrentBiometry, Unattended::WantsTheOwner, None)
    }

    pub fn prompts(&self) -> usize {
        self.prompts.load(Ordering::SeqCst)
    }

    /// A blob of this enclave's access, whatever its gate would say to it.
    pub fn blob(&self) -> Vec<u8> {
        let secret = SecretKey::random(&mut rand_core::OsRng);
        let ock = match self.access {
            Access::CurrentBiometry => set(&[entry("cbio", set(&[entry("pbioc", tlv(4, &[1; 16])), entry("pbioh", tlv(4, &[2; 32]))]))]),
            Access::Nothing => tlv(1, &[1]),
        };
        let acl = set(&[entry("ock", ock), entry("oa", tlv(1, &[1]))]);
        let root = set(&[entry("ed", set(&[entry("acl", acl)])), entry("sk", tlv(4, &secret.to_bytes()))]);
        tlv(4, &root)
    }

    fn secret(blob: &[u8]) -> SecretKey {
        SecretKey::from_slice(&blob[blob.len() - 32..]).unwrap()
    }
}

impl Hardware for SoftEnclave {
    fn create(&self) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        Ok(Zeroizing::new(self.blob()))
    }

    fn public_key(&self, blob: &[u8]) -> anyhow::Result<[u8; 65]> {
        Ok(Self::secret(blob).public_key().to_encoded_point(false).as_bytes().try_into().unwrap())
    }

    fn unattended(&self, _: &[u8]) -> Unattended {
        self.unattended
    }

    fn ecdh(&self, blob: &[u8], peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        if let Some(code) = self.refuse {
            return Err(code);
        }
        let peer = PublicKey::from_sec1_bytes(peer).map_err(|_| -3)?;
        let shared = diffie_hellman(Self::secret(blob).to_nonzero_scalar(), peer.as_affine());
        let mut out = Zeroizing::new([0u8; 32]);
        out.copy_from_slice(shared.raw_secret_bytes().as_ref());
        Ok(out)
    }
}
