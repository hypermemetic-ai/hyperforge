# Enrolling

Three things get enrolled: the owner's key on the Mac, a recovery key offline,
and each secret. Commands below are the procedure, to be run by the owner on
their own machines. The end-to-end suite covers this flow with a software age identity
standing in for the Secure Enclave; step 1 says how to run it once with the real one.

Every step uses this crate's binaries. There is no `age` CLI to install, and nothing
comes from Homebrew. On the Mac the build needs Apple's Command Line Tools and Rust:
`xcode-select --install` gives `swiftc`, which compiles the enclave shim.

**Do not enrol a real secret until the round trip in step 1 has passed on this Mac.**

## 1. The owner's key, on the Mac

A key that cannot leave the Secure Enclave, whose every decrypt fires Touch ID. It
needs a Mac with Touch ID and a fingerprint enrolled. No signing identity is needed.

```sh
cargo build --release --no-default-features -p hyperforge-vault
vault-approve enrol --out ~/.config/vault/owner.identity
```

It prints the recipient, `age1tag1…`, for step 3. The file holds `AGE-VAULT-ENCLAVE-1…`,
a wrapped key that only this Mac's enclave can use: it is created owner-only and
never over an existing file. Touch ID does not fire here. It fires at each decrypt.

To ask whether a file still passes, also without firing Touch ID:

```sh
vault-approve enrol-check --identity ~/.config/vault/owner.identity
```

A key passes only if its access control is `current-biometry` and nothing else, and
the enclave itself will not use it unattended but does know it as its own. A key
without the gate, a damaged one, or another Mac's is refused, and refused again at
every decrypt. No command makes a key without the gate.

**Adding or removing a fingerprint ends this key, for good.** That is what
`current-biometry` means: someone who adds their finger to your Mac gets nothing. So do
a wipe, or a new Mac. Then only the recovery key opens your secrets: recover each one,
`vault-approve enrol` again, and re-encrypt under the new recipient. See "Rotating a
secret" in the README. This is why step 2 is not optional.

**Try it once, with nothing real.** This enrols a throwaway key in a temporary
directory, runs a throwaway server with a fake WorkOS over loopback, encrypts with
`vault-encrypt` and decrypts with `vault-approve`, and fires Touch ID once. It needs no
deployed vault and no `vault-login`, and leaves nothing behind:

```sh
PLEXUS_NO_REGISTRY=1 cargo test -p hyperforge-vault --test e2e -- --ignored --exact a_real_enclave_round_trip --nocapture
```

## 2. The recovery key, offline

```sh
vault-approve recovery-keygen --out recovery.key
```

It prints the recipient, `age1…`, and nothing else: the secret goes only to the
file, which is created owner-only and never over an existing one. Put `recovery.key`
on paper or an offline drive, then remove it from this machine. Without it, a lost
Mac loses every secret.

The key alone is not enough to recover: see step 5.

## 3. The recipients file, on the Mac

One recipient per line, as many as you want. A line `vault-encrypt` cannot read
refuses the whole file. `~/.config/vault/recipients.txt`:

```
# owner, Secure Enclave, from step 1
age1tag1...
# recovery, offline
age1...
```

`vault-encrypt` warns if this file names only one.

## 4. The server

`config/production.env` holds who the vault serves: the WorkOS client and the owner's
user id, both public. On the server:

```sh
set -a; . vault/config/production.env; set +a
hyperforge-vault --bind <address> --state-dir /path/to/state \
  --machine-credentials /path/to/machines
```

The default bind is `127.0.0.1:4470`; a tailnet address also works, and anything else
needs `--allow-unsafe-bind`. Without both the client id and the owner's user id the server
still runs, and refuses everyone.

## The owner's sign-in, on the Mac

Register this redirect URI on the WorkOS client first, byte for byte:

```
http://127.0.0.1:4471/callback
```

Then, with the same two values in the environment:

```sh
set -a; . vault/config/production.env; set +a
vault-login
```

It opens the browser, listens on that address, and writes `~/.config/vault/login.json`,
owner-only. `--provider` picks the sign-in when `authkit` is not what the environment uses,
and `--redirect-uri` changes the URI. Sign in again whenever a tool says to run `vault-login`.

## A machine caller

On the machine, make a token and print its hash. Only the hash goes to the server:

```sh
(umask 077; openssl rand -hex 32 > ~/.config/vault/machine.credential)
tr -d '\n' < ~/.config/vault/machine.credential | sha256sum | cut -d' ' -f1
```

On the server, add a line to the `--machine-credentials` file, then restart it:

```
# name, then the sha256 of its token
publish <hash>
```

A machine may `request` and read its own `result`, nothing else. `vault-run` and `vault-get`
take the token with `--credential-file` or `VAULT_CREDENTIAL_FILE`.

## 5. Each secret

From the Mac, with the value on stdin. `vault-encrypt` encrypts it in its own
process, through one zeroized buffer, and writes it nowhere:

```sh
export VAULT_URL=ws://<address>:4470
export VAULT_MANIFEST=~/.config/vault/manifest.json
set -a; . vault/config/production.env; set +a
vault-encrypt npm-token ~/.config/vault/recipients.txt < /path/to/value
```

This also records the ciphertext's hash in the manifest, which the approver
checks before any decrypt. Enrol from the machine that approves: a secret the
manifest does not hold is refused.

**Copy `manifest.json` to where the recovery key is, after every secret you
enrol.** It lives on the Mac, and recovery checks every ciphertext against it, so
a lost Mac with no copy of it means a recovery key that opens nothing. It is not
secret: it holds names and hashes. A copy older than a secret, or than its
rotation, refuses that secret.

Avoid `echo value | …` in an interactive shell, which leaves it in your history.
Names are 1 to 64 of `A-Z a-z 0-9 . _ -`, starting with a letter or digit, and
are never overwritten: see "Rotating a secret" in the README.

## 6. The approver

Leave this running on the Mac:

```sh
vault-approve --identity ~/.config/vault/owner.identity --server "$VAULT_URL" \
  --manifest ~/.config/vault/manifest.json
```

Or add `--once` to handle one request and exit: zero if it was approved or denied,
non-zero otherwise.

## 7. Check it

With a throwaway secret, never a real one. On any caller:

```sh
vault-run enrol-check --purpose "enrolment check" --env CHECK -- sh -c 'test -n "$CHECK" && echo ok'
```

The approver shows the request, Touch ID fires on `y`, and the command prints
`ok`. Answering `n` makes `vault-run` exit with `the owner denied the request`.

## Recovery

Recovery needs this crate's `vault-approve` and nothing else: no `age` tool, no
server, no network and no WorkOS sign-in. Build the client half, and bring three files to a machine
you trust:

- the offline `recovery.key`,
- your copy of `manifest.json` (step 5),
- `secrets/<name>.age`, copied from the server.

```sh
vault-approve recover <name> --ciphertext <name>.age \
  --identity recovery.key --manifest manifest.json > /path/you/chose
```

The value goes to stdout and nowhere else: there is no option to write a file, and
nothing is logged. The redirect above is your decision, and the file it makes is
plaintext on disk. To feed a command, pipe it instead.

- A terminal is refused, as the value would stay in the scrollback. Pass
  `--allow-terminal` to accept that.
- A ciphertext whose sha256 is not the manifest's entry for `<name>` is refused
  before the key is read, as is a name with no entry or a missing manifest.
- A file cut short gives an error and no part of the value.
- Memory that cannot be locked gives a warning here, though it stops `vault-approve`
  at its work and `vault-encrypt`; `--require-memory-lock` makes it fatal here too. Prefer to run this somewhere with encrypted swap, or none.

Every ciphertext is plain age format, so any age tool still opens it with this key,
if you have reason to skip the hash check. That is your call and not this command's.
