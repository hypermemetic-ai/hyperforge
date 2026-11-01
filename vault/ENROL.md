# Enrolling

Three things get enrolled: the owner's key on the Mac, a recovery key offline,
and each secret. Commands below are the procedure, to be run by the owner on
their own machines. Nothing here has been run for real yet: the end-to-end suite
covers this flow with a software age identity standing in for the Secure Enclave.

## 1. The owner's key, on the Mac

Needs `age` and `age-plugin-se` on `PATH`.

```sh
mkdir -p ~/.config/vault && chmod 700 ~/.config/vault
age-plugin-se keygen --access-control=current-biometry -o ~/.config/vault/owner.identity
```

It prints the recipient, `age1se1…`. The identity file is only a handle: the key
cannot leave the Secure Enclave, and a decrypt fires Touch ID.

## 2. The recovery key, offline

```sh
age-keygen -o recovery.key
```

It prints the recipient, `age1…`. Put `recovery.key` on paper or an offline drive,
then remove it from this machine. Without it, a lost Mac loses every secret.

## 3. The recipients file, on the Mac

Both recipients, one per line. `~/.config/vault/recipients.txt`:

```
# owner, Secure Enclave
age1se1...
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

From the Mac, with the value on stdin. `age` reads stdin itself, so the plaintext
never passes through `vault-encrypt`:

```sh
export VAULT_URL=ws://<address>:4470
export VAULT_MANIFEST=~/.config/vault/manifest.json
set -a; . vault/config/production.env; set +a
vault-encrypt npm-token ~/.config/vault/recipients.txt < /path/to/value
```

This also records the ciphertext's hash in the manifest, which the approver
checks before any decrypt. Enrol from the machine that approves: a secret the
manifest does not hold is refused.

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

On a machine with the offline key, copy `secrets/<name>.age` from the server:

```sh
age -d -i recovery.key < <name>.age
```
