# p256tag spike

Unreviewed spike code, kept so the source is not lost. Nothing here is part of
hyperforge-vault: it is its own Cargo workspace, excluded from the hyperforge one.

Touch ID is not confirmed. Do not treat this as evidence that the Secure Enclave
flow works, or as a design for the `KeyFormat` the vault still needs.

- `p256tag-core`: an age `p256tag` identity over a `KeyAgreement` callback.
- `vault-demo`: a Mac host for it, with a Swift shim over the enclave.

`vault-demo` writes key blobs to `vault/spike/blobs`, which is ignored, as is `target`.
