//! Mac demo host. Only this file and the shim know about processes, files, or the enclave.
use p256tag_core::{open, recipient_string, seal, KeyAgreement, P256TagIdentity};
use std::{path::PathBuf, process::exit, sync::atomic::{AtomicBool, Ordering}, time::Duration};
use zeroize::Zeroizing;
extern "C" {
    fn se_available() -> i32;
    fn se_create(bio: i32, blob: *mut u8, cap: usize, len: *mut usize, pubk: *mut u8) -> i32;
    fn se_public_key(blob: *const u8, n: usize, pubk: *mut u8) -> i32;
    fn se_ecdh(blob: *const u8, n: usize, peer: *const u8, mode: i32, out: *mut u8) -> i32;
    fn se_cancel();
}
const FORBID_UI: i32 = 0; const PROMPT: i32 = 1; const INVALIDATED: i32 = 2;
const DEFAULT_TIMEOUT: u64 = 45;
const DUMMY: &str = "dummy vault secret";
static TIMED_OUT: AtomicBool = AtomicBool::new(false);

struct Enclave { blob: Vec<u8>, public: [u8; 65], mode: i32 }
impl Enclave {
    fn load(blob: Vec<u8>, mode: i32) -> Self {
        let mut public = [0u8; 65];
        let rc = unsafe { se_public_key(blob.as_ptr(), blob.len(), public.as_mut_ptr()) };
        if rc != 0 { eprintln!("FAIL: cannot read public key, code {rc}"); exit(1); }
        Self { blob, public, mode }
    }
}
impl KeyAgreement for Enclave {
    fn public_key(&self) -> [u8; 65] { self.public }
    fn ecdh(&self, peer: &[u8; 65]) -> Result<Zeroizing<[u8; 32]>, i32> {
        let mut z = Zeroizing::new([0u8; 32]);
        match unsafe { se_ecdh(self.blob.as_ptr(), self.blob.len(), peer.as_ptr(), self.mode, z.as_mut_ptr()) } {
            0 => Ok(z), rc => Err(rc),
        }
    }
}
fn create(gated: bool) -> (Vec<u8>, [u8; 65]) {
    let (mut b, mut p, mut n) = (vec![0u8; 4096], [0u8; 65], 0usize);
    let rc = unsafe { se_create(gated as i32, b.as_mut_ptr(), b.len(), &mut n, p.as_mut_ptr()) };
    if rc != 0 { println!("FAIL: key creation, code {rc}"); exit(1); }
    b.truncate(n); (b, p)
}
fn blob_dir() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../blobs") }

/// Decrypt, expecting either success or refusal, without user interaction.
fn nonint(blob: &str, input: &str, mode: i32) -> (Result<Vec<u8>, String>, Option<i32>) {
    let id = P256TagIdentity::new(Enclave::load(std::fs::read(blob).unwrap(), mode));
    let r = open(&std::fs::read(input).unwrap(), &id).map_err(|e| e.to_string());
    (r, id.last_error())
}
fn watchdog(secs: u64, path: PathBuf) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        TIMED_OUT.store(true, Ordering::SeqCst);
        unsafe { se_cancel() };
        std::thread::sleep(Duration::from_secs(3));
        let _ = std::fs::remove_file(&path);
        println!("FAIL: timed out after {secs}s"); exit(3);
    });
}
fn biometry(secs: u64) -> ! {
    let path = blob_dir().join(format!("biometry-run-{}.bin", std::process::id()));
    let (blob, public) = create(true);
    std::fs::write(&path, &blob).unwrap();
    watchdog(secs, path.clone());
    let ct = seal(&recipient_string(&public).unwrap(), DUMMY.as_bytes()).unwrap();
    println!("encrypted {} bytes. Touch ID prompt: authenticate within {secs}s.", ct.len());
    let id = P256TagIdentity::new(Enclave::load(blob, PROMPT));
    let r = open(&ct, &id);
    let _ = std::fs::remove_file(&path);
    if TIMED_OUT.load(Ordering::SeqCst) { println!("FAIL: timed out after {secs}s"); exit(3); }
    match r {
        Ok(pt) if pt == DUMMY.as_bytes() => { println!("PASS: decrypted through Touch ID"); exit(0) }
        Ok(_) => { println!("FAIL: wrong plaintext"); exit(1) }
        Err(e) => { println!("FAIL: not decrypted ({e}), code {:?}", id.last_error()); exit(1) }
    }
}
fn main() {
    if unsafe { se_available() } != 1 { println!("FAIL: no Secure Enclave"); exit(1); }
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["keygen", gate @ ("none" | "bio"), path] => {
            let (b, p) = create(*gate == "bio");
            std::fs::write(path, b).unwrap();
            println!("{}", recipient_string(&p).unwrap());
        }
        ["encrypt", recipient, text, out] => {
            std::fs::write(out, seal(recipient, text.as_bytes()).unwrap()).unwrap();
            println!("wrote {out}");
        }
        ["decrypt", blob, input] => match nonint(blob, input, FORBID_UI) {
            (Ok(pt), _) => println!("plaintext: {}", String::from_utf8_lossy(&pt)),
            (Err(e), c) => { println!("FAIL: {e}, code {c:?}"); exit(1) }
        },
        ["gate-check", blob, input] => match nonint(blob, input, FORBID_UI) {
            (Ok(_), _) => { println!("FAIL: gated key decrypted without biometry"); exit(1) }
            (Err(e), c) => println!("PASS: refused non-interactively ({e}), code {c:?}"),
        },
        ["refusal-check", blob, input] => match nonint(blob, input, INVALIDATED) {
            (Ok(_), _) => { println!("FAIL: decrypted with invalid context"); exit(1) }
            (Err(e), c) => println!("PASS: dead context fails closed ({e}), code {c:?}"),
        },
        ["recipient", blob] => println!("{}", recipient_string(&Enclave::load(std::fs::read(blob).unwrap(), FORBID_UI).public).unwrap()),
        ["watchdog-check"] => { // no enclave call: a stuck operation must still exit 3
            watchdog(1, blob_dir().join("does-not-exist"));
            std::thread::sleep(Duration::from_secs(30)); println!("FAIL: watchdog did not fire"); exit(1)
        }
        ["--biometry"] => biometry(DEFAULT_TIMEOUT),
        ["--biometry", "--timeout-secs", n] => biometry(n.parse().unwrap()),
        _ => { eprintln!("usage: keygen none|bio BLOB | encrypt AGE1TAG TEXT OUT | decrypt BLOB IN | gate-check BLOB IN | refusal-check BLOB IN | --biometry [--timeout-secs N]"); exit(2) }
    }
}
