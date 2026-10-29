# ORGS epic checkpoint

Snapshot as of 2026-04-18, after ORGS-2 (`orgs_add`) and ORGS-3
(`orgs_update`) landed. Generated while implementing ORGS-4; every row
below is backed by an E2E test in `tests/e2e_user_stories.rs` that
exercises the shipped service surface end-to-end (JSON in via
`DynamicService::route` → service → disk).

## User stories

| ID   | Story                                   | Status | Note |
|------|-----------------------------------------|--------|------|
| US-1 | Onboard a new org with SSH + workspace  | 🟢     | `orgs_add` persists all fields; `orgs_list` round-trips them. |
| US-2 | Rotate an SSH key (merge semantics)     | 🟢     | `orgs_update` default-merges ssh: the rotated key lands, other forges are untouched. |
| US-3 | Re-home and then clear `workspace_path` | 🟢     | Three-intent handling works: dry-run does not persist, set writes the new value, empty-string clears the field and `skip_serializing_if` drops it from the TOML. |
| US-4 | Preview before writing (`dry_run`)      | 🟢     | `orgs_add --dry_run true` emits a preview event referencing the org name and writes nothing. |
| US-5 | Scripted idempotent bootstrap           | 🟡     | Works when the caller guards with `orgs_list` (shell: `jq -e`). `orgs_add` itself is not idempotent — a second call without the guard returns `OrgAddFailed { reason: AlreadyExists }`. Recoverable for scripts, but there is no native `--if-not-exists` flag; see re-pitch. |
| US-6 | `--help` discoverability                | 🟢     | Tested via the service's `JsonSchema`: both `orgs_add` and `orgs_update` expose `ssh` as an object with typed `github`/`codeberg`/`gitlab` fields. Synapse derives per-forge `--ssh.<forge>` flags from exactly this shape. |
| Anti | Rename via `orgs_delete` + `orgs_add`   | 🟢     | Composition works. The anti-story is honored — no rename RPC exists and none is needed. |

## Deferred / discovered

- **Token lifecycle** still lives in the auth sidecar
  (`~/.config/hyperforge/secrets.yaml`). Onboarding a new org still
  requires a second RPC hop to `secrets.auth.set_secret`. This is not a
  regression from the epic's pre-state; it was explicitly out of scope
  in ORGS-1. Worth revisiting if token setup becomes the next friction
  point after this epic.
- **`owner_type: user|org`** is not present on `OrgConfig`. ORGS-1
  flagged this as out of scope. Still out of scope; handling it is a
  separate epic if/when an adapter surface requires the distinction.
- **Bulk / declarative multi-org import** was explicitly out of scope.
  `repo import` already covers the repo level. No signal yet that an
  org-level bulk flow is needed.
- **Native idempotent `orgs_add`** (a `--if-not-exists` flag, or a
  `status: created | existed` result) did not ship. Scripts must use
  the `orgs_list` guard pattern shown in US-5. See re-pitch.
- **Rename RPC** was deliberately declined in ORGS-1; the anti-story
  test confirms `orgs_delete` + `orgs_add` composes cleanly. No
  follow-up needed.

## Re-pitch note

Org CRUD is **done for now** for the "one developer, few orgs" case
this epic was scoped for. All seven user stories compose, and the only
non-green row (US-5) is a known shape — idempotency was framed as a
caller-side composition, not a service-side feature, and the ORGS-4 ticket
template anticipated exactly this as a yellow. The service's error
discriminator (`OrgAddFailureReason::AlreadyExists`) is sufficient for
a script to detect duplicates and move on. If a future consumer emerges
that needs bulk declarative import or in-protocol idempotency (e.g. a
provisioning tool that doesn't want to read-then-write), a small
follow-up epic — call it `ORGS-BULK` or `ORGS-DECL` — could ship a
`orgs_add_if_not_exists` variant or a manifest-based `orgs_apply`. No
such consumer exists today, so the right move is to wait until one does
rather than speculate a shape now.
