# TRANSPORT — Epic Checkpoint

State of the epic as of TRANSPORT-3 (2026-04-18).

## Scope recap

TRANSPORT shipped transport control over the remote-URL shape
(SSH `git@host:org/repo.git` ↔ HTTPS `https://host/org/repo.git`) as a
first-class capability on hyperforge-registered repos: opt-in at init
time, idempotent switch after init, readable via `repo status`, and
compatible with repos that predate the epic. No credential helpers, no
SSH key lifecycle, no new forges — those stay out of scope.

## Automated verification

`tests/e2e_transport.rs` compiles each scenario into a `#[tokio::test]`
that drives the shipped Rust service surface through `DynamicService::route` —
the same code path a `synapse … repo set_transport …` call reaches.
Assertions are on end-states (`git remote -v` output, `repo status`
events), not intermediate event strings, so the tests survive
refactors.

```
$ cargo test --test e2e_transport
running 7 tests
test test_ts1_init_ssh                 ... ok
test test_ts2_init_https               ... ok
test test_ts3_switch_ssh_to_https      ... ok
test test_ts4_switch_https_to_ssh      ... ok
test test_ts5_idempotent_noop          ... ok
test test_ts6_recovery_workflow        ... ok
test test_ts7_pre_existing_repo        ... ok
test result: ok. 7 passed; 0 failed; 0 ignored
```

Each test runs to completion in isolation, and running the full file
twice back-to-back is green both times (no state leaks between
invocations — each scenario owns its own `TempDir` and synthesises its
own git repo inside it).

## State-of-the-epic map

Each user story (expressed as a TS-* scenario in TRANSPORT-3) maps to
a row. Green = the scenario passes end-to-end and the behaviour
composes from shipped primitives without manual glue.

| Story | Status | Notes |
|---|---|---|
| TS-1 init with SSH (default) | green | Existing default preserved; `repo status` emits `RepoTransport { transport: Some(Ssh) }`. |
| TS-2 init with HTTPS | green | `--transport https` produces `https://github.com/<org>/<repo>.git` — no credentials embedded. |
| TS-3 switch SSH → HTTPS | green | `repo set_transport` rewrites `origin` in one call; subsequent `repo status` reflects the change. |
| TS-4 switch HTTPS → SSH | green | Inverse of TS-3; same shape. |
| TS-5 idempotent no-op | green | Second switch to the same target emits `TransportUnchanged`, `.git/config` mtime unchanged — no `git remote set-url` invoked. |
| TS-6 recovery workflow | green | Reproduces the incident that motivated the epic: HTTPS remote, `repo init` rewrites to SSH, `set_transport --https` repairs it in one call. No SSH side-effects. |
| TS-7 pre-existing repo | green | A hand-crafted "legacy" config (no transport field) reports transport correctly via `repo status` and can be switched without re-init. Live read from `git remote` is doing the work. |

All seven rows are green. No yellows, no reds.

## Deferred / discovered

**Deferred (and staying deferred until a real workflow surfaces):**

- Transport switching for codeberg and gitlab forges. TRANSPORT-1's
  Out-of-scope list already lists these; the code paths naturally
  extend (`build_remote_url_with` is forge-generic — codeberg and
  gitlab already produce correct URLs for both transports in unit
  tests) but there's no test repo on this machine that proves the
  end-to-end path. A follow-up epic can flip this on the first time
  a real codeberg or gitlab workflow shows up.
- Mixed-transport-per-forge state (e.g. github over SSH, codeberg over
  HTTPS on the same repo). Today `set_transport` applies the target
  uniformly to every configured forge's remote. If a caller legitimately
  needs split transports they can still drive `git remote set-url`
  directly for the odd one out — at the cost of re-reading transport
  via `repo status`.
- Changing `repo init`'s default transport from SSH to HTTPS. This
  epic deliberately does not do that; it adds opt-in. Whether the
  default ever changes is a product decision and a separate ticket.
- Credential flows (SSH agent, `gh` helper, etc.). Explicitly out of
  scope per TRANSPORT-1. HTTPS URLs are plain and unauthenticated —
  credential availability is the caller's responsibility.

**Discovered during implementation:**

- The existing `git::Transport` enum (from a prior HTTPS-transport
  bootstrap commit, `eadbe2f`) was already the right type. Making it
  `Deserialize + JsonSchema` was enough to give Synapse a closed-set
  `--transport` flag for free. No new type needed, no new
  serialization format.
- `MaterializeOpts` was the natural place to thread the init-time
  transport choice — it already carried dry-run/hooks/ssh-wrapper
  flags, and `materialize()` already owned remote reconciliation.
  `None` preserves today's env-default behaviour so no existing
  caller had to change.
- Storing transport anywhere (config.toml, LocalForge) was tempting
  but unnecessary: the ticket's "pre-existing repo" criterion
  essentially mandates reading live from `git remote`. Doing that
  uniformly is simpler than a cache + fallback and automatically
  handles the legacy-repo case with zero migration code.

## Re-pitch

**Done for now.** TRANSPORT delivered the capability that motivated
the epic — callers can choose SSH or HTTPS at init, flip it later,
and inspect current state, all without leaving hyperforge. The
recovery workflow (TS-6) that forced the epic into existence is
covered. Out-of-scope items remain out-of-scope because no workflow
demanding them has surfaced; if one does, the follow-up epic is
small (extend the same primitives to codeberg/gitlab, or re-examine
the default).
