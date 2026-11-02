//! Who may reach the vault, and who may answer for the owner: the seam behind
//! every method of `Store`.
//!
//! Two questions, kept apart. *Who is this socket?* is the transport's: a
//! [`SessionValidator`] reads the credential on the WebSocket upgrade and
//! leaves an [`AuthContext`] on the connection. [`VaultValidator`] is that
//! validator: a WorkOS token verified by `OidcAdapter`, or a machine
//! credential. It refuses every other identity at the door, which is the
//! allow-list: Menger's WorkOS client is shared, so a valid token proves only
//! that someone is a Menger user.
//!
//! *May it do this?* is [`OwnerGate`]'s, on every call, and it alone makes an
//! [`OwnerProof`] or a [`Requester`], which `Store` requires. So a path to the
//! store that skips the check does not compile. Being let in is not being the
//! owner: a machine passes the door and never the owner's half of the gate.
//!
//! The identity is the only thing a method can read. Over WebSocket the service
//! takes `AuthContext` from the connection's extensions and routes with no raw
//! context (`DynamicService::arc_into_rpc_module` calls `route`, which passes
//! `raw_ctx: None`), so the peer address and the headers never reach a method;
//! "loopback only" cannot be enforced here. That is why machines hold a
//! credential.
//!
//! An unconfigured gate refuses everyone, machines too, and that is the default.

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use plexus_auth_core::adapters::{Carriers, OidcAdapter};
use plexus_auth_core::identity::{AuthenticatorChain, Issuer, Principal};
use plexus_core::plexus::{AuthContext, SessionValidator};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::sync::Arc;

/// How a machine's name is spelled in its principal.
const MACHINE_PREFIX: &str = "vault-machine-";

/// Proof this call came from the owner. Only [`OwnerGate::admit`] and
/// [`OwnerGate::admit_requester`] make one.
#[derive(Debug)]
pub struct OwnerProof {
    _sealed: (),
}

/// Who may ask for a secret and collect the answer.
#[derive(Debug)]
pub enum Requester {
    Owner(OwnerProof),
    Machine(MachineProof),
}

/// Proof this call came from a known machine. Only
/// [`OwnerGate::admit_requester`] makes one.
#[derive(Debug)]
pub struct MachineProof {
    name: String,
}

impl Requester {
    /// The machine asking, if it is one.
    pub fn machine(&self) -> Option<&str> {
        match self {
            Requester::Owner(_) => None,
            Requester::Machine(proof) => Some(&proof.name),
        }
    }

    /// Whether this caller may read a request made by `asked`. The owner reads
    /// anything; a machine reads only what it asked for itself.
    pub fn may_read(&self, asked: Option<&str>) -> bool {
        match self {
            Requester::Owner(_) => true,
            Requester::Machine(proof) => asked == Some(proof.name.as_str()),
        }
    }
}

/// The owner, if anyone has said who that is.
#[derive(Clone, Debug, Default)]
pub struct OwnerGate {
    owner: Option<Principal>,
}

impl OwnerGate {
    /// Refuses everyone.
    pub fn unconfigured() -> Self {
        OwnerGate { owner: None }
    }

    pub fn owned_by(owner: Principal) -> Self {
        OwnerGate { owner: Some(owner) }
    }

    /// Whether anyone has said who the owner is.
    pub fn is_configured(&self) -> bool {
        self.owner.is_some()
    }

    /// The caller's principal, if there is an owner at all, and the caller is
    /// someone whose sign-in still stands. A socket can outlive its token by up
    /// to the transport's liveness tick; a call on it cannot.
    fn principal(&self, caller: Option<&AuthContext>) -> anyhow::Result<(Principal, Option<Principal>)> {
        let Some(owner) = &self.owner else {
            bail!("no owner is configured: this method is refused");
        };
        let caller = caller.filter(|c| !c.expired()).and_then(AuthContext::principal);
        Ok((owner.clone(), caller))
    }

    /// The proof, if `caller` is the owner. Anything less is a refusal: no
    /// configured owner, no identity on the connection, an identity with no
    /// principal, an expired one, or somebody else's.
    pub fn admit(&self, caller: Option<&AuthContext>) -> anyhow::Result<OwnerProof> {
        let (owner, caller) = self.principal(caller)?;
        if caller.as_ref() != Some(&owner) {
            bail!("this method is the owner's");
        }
        Ok(OwnerProof { _sealed: () })
    }

    /// The right to ask and to collect: the owner, or a machine. Machine
    /// principals only ever come from [`VaultValidator`].
    pub fn admit_requester(&self, caller: Option<&AuthContext>) -> anyhow::Result<Requester> {
        let (owner, caller) = self.principal(caller)?;
        match caller {
            Some(principal) if principal == owner => Ok(Requester::Owner(OwnerProof { _sealed: () })),
            Some(principal) => match machine_name(&principal) {
                Some(name) => Ok(Requester::Machine(MachineProof { name: name.to_string() })),
                None => bail!("this method is the owner's"),
            },
            None => bail!("this method is the owner's"),
        }
    }
}

fn machine_name(principal: &Principal) -> Option<&str> {
    (principal.issuer() == Issuer::ApiKey).then(|| principal.subject().strip_prefix(MACHINE_PREFIX)).flatten()
}

/// `oidc:workos/<user id>`, the owner's principal.
pub fn workos_principal(user_id: &str) -> anyhow::Result<Principal> {
    Principal::oidc("workos", user_id).map_err(|e| anyhow!("the owner's user id: {e}"))
}

/// A machine caller: a name, and the sha256 of its token. The token is
/// generated on the machine and only its hash reaches the server, so the
/// server's disk grants nothing. It must be high-entropy for that to mean
/// anything (ENROL.md).
pub struct MachineCredential {
    name: String,
    digest: [u8; 32],
}

impl MachineCredential {
    /// One per line, `<name> <sha256 in hex>`; `#` starts a comment. Anything
    /// else is a startup error, not a machine that quietly cannot get in.
    pub fn parse_list(text: &str) -> anyhow::Result<Vec<MachineCredential>> {
        let mut machines = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split_whitespace();
            let (Some(name), Some(hex), None) = (fields.next(), fields.next(), fields.next()) else {
                bail!("line {}: expected a name and a hash", n + 1);
            };
            machines.push(Self::new(name, hex).with_context(|| format!("line {}", n + 1))?);
        }
        Ok(machines)
    }

    fn new(name: &str, hex: &str) -> anyhow::Result<Self> {
        if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            bail!("a machine's name is 1 to 64 of A-Z a-z 0-9 - _");
        }
        if hex.len() != 64 || !hex.is_ascii() {
            bail!("the machine credential hash is not 64 hex digits");
        }
        let mut digest = [0u8; 32];
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).context("not hex")?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| anyhow!("the machine credential hash is not 64 hex digits"))?;
        }
        Ok(MachineCredential { name: name.to_string(), digest })
    }

    /// Who this credential proves.
    fn principal(&self) -> anyhow::Result<Principal> {
        Principal::new(Issuer::ApiKey, format!("{MACHINE_PREFIX}{}", self.name)).map_err(|e| anyhow!("the machine's name: {e}"))
    }
}

/// The door. Machine tokens by hash; anything else is offered to WorkOS's
/// adapter, and survives only as the owner.
pub struct VaultValidator {
    admitted: Option<Admitted>,
}

struct Admitted {
    oidc: Carriers,
    owner: Principal,
    machines: Vec<(Principal, [u8; 32])>,
}

impl VaultValidator {
    /// Nobody gets in: the server has not been told who it serves.
    pub fn refusing() -> Self {
        VaultValidator { admitted: None }
    }

    pub fn new(oidc: OidcAdapter, owner: Principal, machines: Vec<MachineCredential>) -> anyhow::Result<Self> {
        let chain = AuthenticatorChain::new().with(Arc::new(oidc));
        let oidc = Carriers::try_bearer_only(chain).map_err(|e| anyhow!("the WorkOS adapter: {e}"))?;
        let machines = machines.into_iter().map(|m| Ok((m.principal()?, m.digest))).collect::<anyhow::Result<_>>()?;
        Ok(VaultValidator { admitted: Some(Admitted { oidc, owner, machines }) })
    }
}

#[async_trait]
impl SessionValidator for VaultValidator {
    async fn validate(&self, presented: &str) -> Option<AuthContext> {
        let admitted = self.admitted.as_ref()?;
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let mut machine = None;
        for (principal, known) in &admitted.machines {
            // Every byte of every entry: the comparison says nothing by how
            // long it takes.
            if digest.iter().zip(known).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0 {
                machine = Some(principal);
            }
        }
        if let Some(principal) = machine {
            return Some(AuthContext::for_principal(principal, "machine", Vec::new(), serde_json::Value::Null));
        }
        let context = admitted.oidc.validate(presented).await?;
        if context.principal().as_ref() != Some(&admitted.owner) {
            // Say nothing of who: a signed-in stranger is a Menger user.
            tracing::warn!("refused at the upgrade: a valid token, not the owner's");
            return None;
        }
        Some(context)
    }
}

/// Loopback, or a tailnet address: Tailscale's 100.64.0.0/10 and
/// fd7a:115c:a1e0::/48. The token crosses the socket as `ws://`, so the network
/// has to be what protects it. 100.64.0.0/10 is also carrier-grade NAT space;
/// this says the address could be a tailnet's, not that it is.
pub fn bind_is_safe(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback() || (s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(principal: &str) -> AuthContext {
        AuthContext::for_principal(&principal.parse().unwrap(), "s", Vec::new(), serde_json::Value::Null)
    }

    fn workos(user: &str) -> Principal {
        format!("oidc:workos/{user}").parse().unwrap()
    }

    const MACHINE: &str = "apikey:vault-machine-publish";

    #[test]
    fn unconfigured_refuses_everyone() {
        let gate = OwnerGate::default();
        assert!(gate.admit(None).is_err());
        assert!(gate.admit(Some(&context("oidc:workos/user_a"))).is_err());
        assert!(gate.admit(Some(&context(MACHINE))).is_err());
        assert!(gate.admit_requester(None).is_err());
        assert!(gate.admit_requester(Some(&context("oidc:workos/user_a"))).is_err());
        assert!(gate.admit_requester(Some(&context(MACHINE))).is_err());
    }

    #[test]
    fn only_the_owner_is_admitted() {
        let gate = OwnerGate::owned_by(workos("user_a"));
        assert!(gate.admit(Some(&context("oidc:workos/user_a"))).is_ok());
        assert!(gate.admit(Some(&context("oidc:workos/user_b"))).is_err());
        assert!(gate.admit(None).is_err());
        assert!(gate.admit(Some(&AuthContext::anonymous())).is_err());
    }

    #[test]
    fn a_context_with_no_principal_is_nobody() {
        let gate = OwnerGate::owned_by(workos("user_a"));
        let forged = AuthContext::new("oidc:workos/user_a".into(), "s".into(), Vec::new(), serde_json::Value::Null);
        assert!(forged.principal().is_none(), "the premise: a bare user_id names no principal");
        assert!(gate.admit(Some(&forged)).is_err());
        assert!(gate.admit_requester(Some(&forged)).is_err());
    }

    #[test]
    fn an_expired_context_is_nobody() {
        let gate = OwnerGate::owned_by(workos("user_a"));
        let context = |expires: i64| AuthContext::for_principal(&workos("user_a"), "s", Vec::new(), serde_json::json!({"expires_at": expires}));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!(gate.admit(Some(&context(now + 60))).is_ok());
        assert!(gate.admit(Some(&context(now - 1))).is_err());
        assert!(gate.admit_requester(Some(&context(now - 1))).is_err());
    }

    #[test]
    fn a_machine_may_ask_and_is_never_the_owner() {
        let gate = OwnerGate::owned_by(workos("user_a"));
        assert!(gate.admit(Some(&context(MACHINE))).is_err());
        let who = gate.admit_requester(Some(&context(MACHINE))).unwrap();
        assert_eq!(who.machine(), Some("publish"));
        assert!(who.may_read(Some("publish")));
        assert!(!who.may_read(Some("mirror")));
        assert!(!who.may_read(None), "the owner's request is not a machine's to read");
    }

    #[test]
    fn nobody_else_may_ask() {
        let gate = OwnerGate::owned_by(workos("user_a"));
        assert!(gate.admit_requester(None).is_err());
        assert!(gate.admit_requester(Some(&context("oidc:workos/user_b"))).is_err());
        assert!(gate.admit_requester(Some(&context("apikey:some-other-key"))).is_err());
        assert!(gate.admit_requester(Some(&context("oidc:workos/user_a"))).unwrap().may_read(Some("publish")));
    }

    #[tokio::test]
    async fn a_refusing_validator_refuses() {
        assert!(VaultValidator::refusing().validate("anything").await.is_none());
    }

    #[test]
    fn a_malformed_machine_list_is_a_startup_error() {
        let hash = "a".repeat(64);
        for bad in [format!("only-a-name"), format!("x {} extra", hash), format!("x abc"), format!("x {}", "g".repeat(64)), format!("x {}", "é".repeat(32)), format!("bad/name {hash}")] {
            assert!(MachineCredential::parse_list(&bad).is_err(), "{bad}");
        }
        assert_eq!(MachineCredential::parse_list(&format!("# a comment\n\npublish {hash} # trailing\n")).unwrap().len(), 1);
    }

    #[test]
    fn only_loopback_and_tailnet_binds_are_safe() {
        for good in ["127.0.0.1", "127.8.8.8", "::1", "::ffff:127.0.0.1", "100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::1", "::ffff:100.100.1.1"] {
            assert!(bind_is_safe(good.parse().unwrap()), "{good}");
        }
        for bad in ["0.0.0.0", "::", "192.168.1.5", "10.0.0.1", "100.63.255.255", "100.128.0.0", "8.8.8.8", "fd7a:115c:a1e1::1", "fe80::1"] {
            assert!(!bind_is_safe(bad.parse().unwrap()), "{bad}");
        }
    }
}
