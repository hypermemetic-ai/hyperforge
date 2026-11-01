# Enrolling

Three things get enrolled: the owner's key on the Mac, a recovery key offline,
and each secret. Commands below are the procedure, to be run by the owner on
their own machines. Nothing here has been run for real yet: the end-to-end suite
covers this flow with a software age identity standing in for the Secure Enclave.

Every step uses this crate's binaries. There is no `age` CLI to install.

**Do not enrol a real secret yet.** Step 1 is pending, and the steps after it name
its recipient.

## 1. The owner's key, on the Mac

**Pending the Secure Enclave spike.** The vault no longer runs `age` or any
plugin, so an enclave key needs a `KeyFormat` in `src/keys.rs`, and that is not
written. Until it is, `vault-encrypt` refuses an `age1se1…` line and `vault-approve`
refuses an `AGE-PLUGIN-SE-1…` identity, each naming the line, and nothing is
encrypted or decrypted. This step will be written from the spike's result.

The intent stands: a key that cannot leave the Secure Enclave, whose decrypt fires
Touch ID, created with `current-biometry` access control.

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
# owner, Secure Enclave: pending, see step 1
age1se1...
# recovery, offline
age1...
```

`vault-encrypt` warns if this file names only one.

## 4. The owner credential and the server

On the Mac, make the credential and print its hash. Only the hash goes to the
server, and the credential itself never leaves this file:

```sh
(umask 077; openssl rand -hex 32 > ~/.config/vault/owner.credential)
tr -d '\n' < ~/.config/vault/owner.credential | shasum -a 256 | cut -d' ' -f1
```

On the server, with that hash:

```sh
hyperforge-vault --bind <address> --state-dir /path/to/state --owner-credential-hash <hash>
```

The default bind is `127.0.0.1:4470`. Read "Not yet protected" in the README
before binding anything wider. Without `--owner-credential-hash` the server still
runs, and refuses `get_request`, `fulfill` and `deny` for everyone: no request can
be answered.

## 5. Each secret

From the Mac, with the value on stdin. `vault-encrypt` encrypts it in its own
process, through one zeroized buffer, and writes it nowhere:

```sh
export VAULT_URL=ws://<address>:4470
export VAULT_MANIFEST=~/.config/vault/manifest.json
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
  --manifest ~/.config/vault/manifest.json \
  --owner-credential-file ~/.config/vault/owner.credential
```

Or add `--once` to handle one request and exit: zero if it was approved or denied,
non-zero otherwise.

## 7. Check it

With a throwaway secret, never a real one. On any caller:

```sh
vault-run enrol-check --purpose "enrolment check" --env CHECK -- sh -c 'test -n "$CHECK" && echo ok'
```

The approver shows the request, Touch ID fires on `y` (once step 1 exists), and the command prints
`ok`. Answering `n` makes `vault-run` exit with `the owner denied the request`.

## Recovery

Recovery needs this crate's `vault-approve` and nothing else: no `age` tool, no
server and no network. Build the client half, and bring three files to a machine
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
- Memory that cannot be locked gives a warning; `--require-memory-lock` makes it
  fatal. Prefer to run this somewhere with encrypted swap, or none.

Every ciphertext is plain age format, so any age tool still opens it with this key,
if you have reason to skip the hash check. That is your call and not this command's.
