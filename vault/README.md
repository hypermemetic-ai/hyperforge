# hyperforge-vault

A secret vault whose server cannot read what it stores. The server holds only
`age` ciphertext. Each read of a secret is one request, and the only thing that
answers a request is its owner saying yes on their own machine.

```mermaid
sequenceDiagram
  participant C as caller (vault-run / vault-get)
  participant V as vault (server, no key)
  participant O as vault-approve (owner's Mac)
  C->>C: make a one-time key in memory
  C->>V: request(secret, purpose, one-time public key)
  V-->>O: watch: pending request
  O->>O: show it, ask y/N
  O->>V: get_request(id), as the owner
  V-->>O: ciphertext
  O->>O: check its hash against the local manifest
  O->>O: in process: open with the identity, seal to the one-time key
  O->>V: fulfill(id, sealed)
  C->>V: result(id)
  V-->>C: sealed, once, then deleted
  C->>C: open it in memory
```

On a yes, `vault-approve` opens the ciphertext and seals it to the one-time key
itself, through the `age` crate. No `age` binary or plugin runs. The plaintext
crosses one zeroized buffer in that process, and reaches no disk and no child
process. See "Inside the owner's tools" below for what that does and does not
promise. The ciphertext is ordinary age format: any age implementation opens it.

## The pieces

| Binary | Runs on | Does |
|---|---|---|
| `hyperforge-vault` | the server | Stores ciphertext, moves sealed results, writes the audit log. Has no key. |
| `vault-approve` | the owner's Mac | Watches requests, asks, checks the manifest, decrypts in process, reading the identity file at each decrypt. `--once` handles one request and exits. `recovery-keygen --out <file>` writes a recovery identity and prints its recipient. `recover <name>` opens one ciphertext with that identity, to stdout only. |
| `vault-login` | the owner's Mac | Signs the owner in to WorkOS (PKCE, no secret) and keeps the tokens for the two tools around it. |
| `vault-encrypt` | the owner's Mac | Encrypts stdin to a recipients file, stores the ciphertext, records its hash in the manifest. |
| `vault-run` | the caller | Runs a command with the secret in one environment variable. |
| `vault-get` | the caller | Prints the secret to a pipe; a terminal needs `--allow-terminal`. |

Enrolling a machine and a secret: [ENROL.md](ENROL.md).

## Building

```sh
cargo build -p hyperforge-vault                        # everything
cargo build -p hyperforge-vault --no-default-features  # the client half: no server stack
```

The Mac only needs the client half, and so does anything that depends on this
crate to fetch a secret: `default-features = false` gives the `client`, `keys`,
`manifest` and `wire` modules with no plexus-core, plexus-macros or plexus-transport.
On a Mac, either build runs `/usr/bin/swiftc` for the enclave shim, from Apple's Command
Line Tools. No other build runs it.

## Testing

```sh
vault/scripts/test.sh
```

Run from anywhere in the workspace, or `cargo test -p hyperforge-vault --all-features`
with `PLEXUS_NO_REGISTRY=1`. Runs real binaries over loopback and needs no `age` CLI:
the tools' `PATH` holds an `age` that records being run, and that record failing
the suite is how "nothing spawns age" is checked. Each test has a private `HOME`, a
clean environment, throwaway keys and a random canary value. The suite scans state,
logs and process memory for it.

`tests/interop.rs` checks both directions against a real age CLI, when you have one.
It is ignored by default and never fetches anything:

```sh
VAULT_AGE_CLI_DIR=/dir/holding/age PLEXUS_NO_REGISTRY=1 \
  cargo test -p hyperforge-vault --test interop -- --ignored
```

`--all-features` turns on `memory-scan-test`, which lets a test binary keep the
owner's tools dumpable so the suite can read their memory. It is for tests only:
never build a real tool with it, or with `--all-features`.

## The platform layer

`src/platform.rs` is compiled into the binaries by `#[path]` and is no part of the
library: the zeroing allocator, memory locking and core-dump settings are decisions
for a process on a desktop, and the library is what a mobile build shares. TTY
handling lives in the binaries too. `tests/platform.rs` fails if any of these reach
the library. For the same reason recovery's logic is `keys::recover`, which returns
the plaintext to its caller, and only `vault-approve` prints it.

## Keys

`src/keys.rs` is the one place that reads a key. `KeyFormat` turns a line of a
recipients or identity file into the crate's `age::Recipient` or `age::Identity`,
and `FORMATS` lists what this build reads: X25519, and the Secure Enclave. `encrypt`
and `reseal` see only the traits, so a new key type is one more `KeyFormat`.

The crate's `plugin` feature stays off: it would run `age-plugin-*` as a child
process with the file key. So plugin and SSH recipients are refused, not skipped: a
recipients file with a line no format reads encrypts nothing.

The Secure Enclave format is age's own `p256tag`, so `age1tag1…` is a standard recipient
that encrypts anywhere, with `age::tag::Recipient`. Decrypting is three layers. `src/p256tag.rs`
is pure and portable: the stanza and RFC 9180's key schedule, over a `KeyAgreement`
callback. `src/enclave.rs` holds the gate. `shim/seshim.swift` is the enclave, compiled
by `build.rs` with `swiftc` on a Mac and skipped everywhere else, where the identity is
refused by name and the recipient still works. iOS has no shim yet.

`GatedKey::open` is the only way to a key, and refuses one unless its access control
reads as exactly `current-biometry` and the enclave, asked with the interface forbidden,
refuses it for want of the owner. Neither can prompt. The access control is an
undocumented format, read as seen on macOS 14: a change to it refuses the key rather
than passing it, and the offline recovery key is what remains until this code catches
up. The shim stores nothing, uses no keychain, and cannot be asked for an ungated key.
A Touch ID prompt left past the deadline is dismissed, not answered late.
See [ENROL.md](ENROL.md).

## The manifest

A local file on the owner's Mac (`--manifest`, `VAULT_MANIFEST`) mapping each
secret's name to the sha256 of its ciphertext. `vault-encrypt` writes it once the
server has taken the ciphertext. `vault-approve` reads it after a yes and before
any decrypt: a name with no entry, a hash that differs, or a manifest it cannot
read is denied with nothing decrypted, so Touch ID does not fire. It never crosses
the wire, so a server cannot speak for it. Its integrity is the integrity of the
owner's account. A secret stored without `vault-encrypt` on this machine, or
encrypted from another one, is refused until it is enrolled here.

## The gate

Every method is gated. Two checks, kept apart:

- **The door.** The transport asks `owner::VaultValidator` who a socket is, on the
  upgrade, and a socket with no answer gets a 401. It admits the owner's WorkOS token
  and a known machine's credential, and nobody else: no token, a bad signature, another
  client or issuer, an expired token. A valid token for another user is refused too,
  because Menger's WorkOS client is shared: a good token proves only that someone is a
  Menger user.
- **The gate.** Each method then asks `owner::OwnerGate`. `Store` will not run without
  the `OwnerProof` or `Requester` that only the gate makes, so skipping it does not
  compile. With no owner configured it refuses everyone, machines too, and that is the
  default.

| Method | Owner | Machine |
|---|---|---|
| `request` | yes | yes |
| `result` | any request | its own requests; anyone else's reads as no such request |
| `put`, `list`, `pending`, `watch`, `get_request`, `fulfill`, `deny`, `audit` | yes | no |

A method can read the identity and nothing else. Over WebSocket the hub routes with no
raw request context, so the peer address and headers never reach a method; a check on
"came from loopback" is not possible at this layer. That is why machines hold a
credential: a random token, of which the server holds only the sha256.

**The owner's token.** `vault-login` signs in as a public client with PKCE; there is no
secret on this side. synapse's login cannot be reused: it stores an opaque Menger session,
which WorkOS's issuer did not sign. Access tokens are short-lived, and a socket closes when
its token ends. The approver refreshes on each reconnect and answers each request on a
connection of its own, so an answer given while the socket closed is not lost. `--once`
exits non-zero instead of reconnecting.

**The bind.** Tokens cross the socket as `ws://`, so the network has to protect them. The
service refuses to bind anything but loopback or a tailnet address (100.64.0.0/10,
fd7a:115c:a1e0::/48) without `--allow-unsafe-bind`. 100.64.0.0/10 is also carrier-grade NAT
space, so this says the address could be a tailnet's, not that it is.

## The server's state

Under `--state-dir`, all owner-only:

- `secrets/<name>.age`: the ciphertexts.
- `requests/<id>.json`: one request each, replaced atomically on every change. A
  restart loses nothing.
- `audit.jsonl`: append-only: put, request, approve, deny, fulfill, deliver,
  expire. Never a value and never ciphertext.

Requests last `--request-ttl-secs` (300). A sealed result waits
`--result-ttl-secs` (60) for its one read, then goes.

## Rotating a secret

A stored secret is never replaced over the wire: an overwrite is how a device on
the network would swap a secret for a value it chose. Rotation is a deliberate
act on the server: delete `secrets/<name>.age` there, then run `vault-encrypt`
again, which replaces the manifest entry.

## What this does and does not protect

Protected: a read of the server's disk, memory or logs gives ciphertext only. The
server cannot approve for you. A sealed result opens only with the one-time key
of the caller that asked, and reads once. A server, compromised or not, cannot get
a ciphertext decrypted that the manifest does not hold for the name you approved.
A socket without the owner's token or a machine's credential reaches nothing at all.

**Not yet protected.** These are the known gaps, in the order they will close:

- **A machine credential does not expire.** It lasts until its line leaves
  `--machine-credentials` and the server restarts. A stolen one can raise requests,
  which the owner still has to approve.
- **A compromised server can still mislead a caller**, by answering `result`
  itself with anything sealed to the caller's one-time key. The owner gate holds
  against other sockets, not against the server. A caller has nothing that
  authenticates the sealed result.
- **`purpose` and `caller` are the caller's own words.** The prompt says so.
  The server knows which machine asked and uses that for `result`, but neither the
  prompt nor the audit log shows it yet.
- **`login.json` holds a refresh token** in a 0600 file on the Mac. Two tools refreshing
  in the same instant can race on it, since WorkOS's refresh tokens rotate; `vault-login`
  recovers.

Limits that will stay:

- **Inside the owner's tools.** `vault-encrypt` and `vault-approve` hold the value
  in their own memory while they work, which the `age` CLI used to do for them.
  Anything that can read this process's memory during that time can read it. See
  below for what shortens that time and what does not.
- **Same user.** `vault-run` puts the value in the command's environment, where
  another process of the same user can read it from `/proc`. Run what you do not
  trust as a different user.
- **`PLEXUS_WS_TRACE` is a debug tool.** It prints raw frames, so caller-written
  text reaches your terminal unsanitised. Never set it on a real approver.

### Memory in the owner's tools

`vault-approve`, including `recover`, and `vault-encrypt` run on a heap that wipes
every block as it is freed, including the `Vec`s inside the `age` crate that no
`Zeroizing` reaches, and that locks every heap block out of swap. `vault-login`, which
holds the refresh token, has the same heap without the locking. Core dumps are
off in all three: `RLIMIT_CORE` is 0, and on Linux the process is also undumpable, since the
limit does not bind a core piped to a handler such as systemd-coredump. That also
closes `/proc/<pid>/mem` and ptrace to this user's other processes.

If memory cannot be locked, `vault-approve` and `vault-encrypt` stop: before any
value at startup, and with no seal sent if a block fails to lock later. A block that
fails later costs that one request, which is denied: the next is judged on its own
blocks, with no restart. There is no switch to go on regardless, so the remedy is a
higher `ulimit -l`. On a Mac the default is `unlimited`, and both tools run there; below
about 512 KiB they stop.
`--require-memory-lock` is still accepted on both and changes nothing. `vault-approve
recover` is the exception: it says so on stderr and goes on, because a recovery on a
bad day must still work, and there `--require-memory-lock` makes it stop.
`recovery-keygen` also warns and goes on. `mlockall` is not used: it does not exist on
macOS, and on Linux it can make a thread's stack fail to map under a low limit.

Measured: the suite's canary is gone from both tools 300 ms after each of three
releases, at two value sizes, where before it stayed. That is the test, not a proof
of absence.

**Not covered:**

- **Stack copies.** Stack frames are neither wiped nor locked. The scan finds none,
  but nothing prevents one, and a finished thread's stack can be reused unwiped.
- **While the value is in use.** A live value is plaintext in memory until freed.
  On macOS these tools are not undumpable, so what stops a same-user debugger there
  is the system's own policy, not this code.
- **The terminal.** Its scrollback, under `--allow-terminal`, is the terminal's.
- **Kernel buffers.** The pipe into `vault-encrypt` or out of `recover`.
- **The caller's copy after a release.** `vault-get` and `vault-run` have neither the
  heap nor the locking, only `RLIMIT_CORE`, and `vault-run` hands the value to a
  child's environment. Wiping the approver does nothing for those.
- **Hibernation.** A locked page is still written to the hibernation image, under
  full-disk encryption if it is on.
- **The window at startup.** Blocks allocated before `lock_heap` runs are wiped but
  not locked, and on macOS freeing one can take a count off a page it shares with a
  locked block. Nothing secret exists by then.
- **Bytes past a block's requested size.** Only what was asked for is wiped.

## Licence

MIT.
