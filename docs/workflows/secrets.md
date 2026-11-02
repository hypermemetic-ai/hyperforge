# Secrets

Hyperforge resolves `secrets://<path>` from `~/.config/hyperforge/secrets.yaml` (0600, plaintext until the vault replaces it).

```bash
pbpaste | hf secrets set --key secrets://npm/token --value -      # from stdin: never on a command line
hf secrets list_refs                                               # keys only
hf secrets delete --key secrets://npm/token
```

Paths in use: `github/<org>/token`, `github/_default/token`, `codeberg/_default/token`, `cargo/token`, `hackage/token`, `npm/token`, `pypi/token`.

`scripts/auth-walk.py` collects every missing token: it opens each provider's token page, reads the token from the clipboard, checks it, stores it and clears the clipboard.

`hf auth_check` proves forge credentials work; `hf auth_requirements` lists what is missing.
