//! Who may answer for the owner: the seam behind `get_request`, `fulfill` and
//! `deny`.
//!
//! Two questions, kept apart. *Who is this socket?* is the transport's: a
//! [`SessionValidator`] reads the credential on the WebSocket upgrade and
//! leaves an [`AuthContext`] on the connection. *Is that the owner?* is
//! [`OwnerGate`]'s, and it is the only thing that mints an [`OwnerProof`],
//! which the three store methods require. So a path to them that skips the
//! check does not compile.
//!
//! The identity is the only thing a method can read. Over WebSocket the hub
//! takes `AuthContext` from the connection's extensions and routes with no raw
//! context (`DynamicHub::arc_into_rpc_module` calls `route`, which passes
//! `raw_ctx: None`), so the peer address and the headers never reach a method;
//! "loopback only" cannot be enforced here.
//!
//! An unconfigured gate refuses everyone, and that is the default. HYPE-17
//! plugs in by passing its `OidcValidator` as the validator and
//! `OwnerGate::owned_by(<the oidc:workos/user_… principal>)`; nothing else here
//! changes.

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use plexus_auth_core::identity::Principal;
use plexus_core::plexus::{AuthContext, SessionValidator};
use sha2::{Digest, Sha256};

/// Proof this call came from the owner. Only [`OwnerGate::admit`] makes one.
#[derive(Debug)]
pub struct OwnerProof {
    _sealed: (),
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

    /// The proof, if `caller` is the owner. Anything less is a refusal: no
    /// configured owner, no identity on the connection, an identity with no
    /// principal, or somebody else's.
    pub fn admit(&self, caller: Option<&AuthContext>) -> anyhow::Result<OwnerProof> {
        let Some(owner) = &self.owner else {
            bail!("no owner is configured: this method is refused");
        };
        let caller = caller.and_then(AuthContext::principal);
        if caller.as_ref() != Some(owner) {
            bail!("this method is the owner's");
        }
        Ok(OwnerProof { _sealed: () })
    }
}

/// The explicitly configured local credential: until HYPE-17, the only real
/// way to be the owner. The approver holds a random token and presents it as
/// a Bearer token; the server holds its sha256, so its disk grants nothing.
/// The token must be high-entropy for that to mean anything (ENROL.md).
pub struct LocalCredential {
    digest: [u8; 32],
}

impl LocalCredential {
    /// From the token's sha256, in hex. Anything else is a startup error, not
    /// a server that quietly has no owner.
    pub fn from_hex_digest(hex: &str) -> anyhow::Result<Self> {
        let hex = hex.trim();
        if hex.len() != 64 || !hex.is_ascii() {
            bail!("the owner credential hash is not 64 hex digits");
        }
        let mut digest = [0u8; 32];
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).context("not hex")?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| anyhow!("the owner credential hash is not 64 hex digits"))?;
        }
        Ok(LocalCredential { digest })
    }

    /// Who the local credential proves.
    pub fn principal() -> Principal {
        "apikey:vault-local-owner".parse().expect("a constant, valid principal")
    }
}

#[async_trait]
impl SessionValidator for LocalCredential {
    async fn validate(&self, presented: &str) -> Option<AuthContext> {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        // Every byte, always: the comparison says nothing by how long it takes.
        let difference = digest.iter().zip(&self.digest).fold(0u8, |acc, (a, b)| acc | (a ^ b));
        (difference == 0).then(|| AuthContext::for_principal(&Self::principal(), "local", Vec::new(), serde_json::Value::Null))
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

    #[test]
    fn unconfigured_refuses_everyone() {
        let gate = OwnerGate::default();
        assert!(gate.admit(None).is_err());
        assert!(gate.admit(Some(&context("oidc:workos/user_a"))).is_err());
        assert!(gate.admit(Some(&AuthContext::for_principal(&LocalCredential::principal(), "s", Vec::new(), serde_json::Value::Null))).is_err());
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
        let gate = OwnerGate::owned_by(LocalCredential::principal());
        let forged = AuthContext::new("apikey:vault-local-owner".into(), "s".into(), Vec::new(), serde_json::Value::Null);
        assert!(forged.principal().is_none(), "the premise: a bare user_id names no principal");
        assert!(gate.admit(Some(&forged)).is_err());
    }

    #[tokio::test]
    async fn the_local_credential_is_its_token_and_only_its_token() {
        let hex: String = Sha256::digest(b"token").iter().map(|b| format!("{b:02x}")).collect();
        let local = LocalCredential::from_hex_digest(&hex).unwrap();
        let ctx = local.validate("token").await.expect("the token");
        assert!(OwnerGate::owned_by(LocalCredential::principal()).admit(Some(&ctx)).is_ok());
        assert!(local.validate("Token").await.is_none());
        assert!(local.validate("").await.is_none());
        assert!(local.validate(&hex).await.is_none(), "the hash is not the credential");
    }

    #[test]
    fn a_malformed_hash_is_a_startup_error() {
        for bad in ["", "abc", &"g".repeat(64), &"é".repeat(32)] {
            assert!(LocalCredential::from_hex_digest(bad).is_err(), "{bad}");
        }
    }
}
