# Enrolling

Three things get enrolled: the owner's key on the Mac, a recovery key offline,
and each secret. Commands below are the procedure, to be run by the owner on
their own machines. Nothing here has been run for real yet: the end-to-end suite
covers this flow with a software age identity standing in for the Secure Enclave.

## 1. The owner's key, on the Mac

Needs `age` and `age-plugin-se` on `PATH`.

```sh
mkdir -p ~/.config/vault && chmod 700 ~/.config/vault
age-plugin-se keygen --access-control=any-biometry -o ~/.config/vault/owner.identity
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

```sh
hyperforge-vault --bind <address> --state-dir /path/to/state
```

The default bind is `127.0.0.1:4470`. Read "Not yet protected" in the README
before binding anything wider: the address is the only gate today.

## 5. Each secret

From the Mac, with the value on stdin. `age` reads stdin itself, so the plaintext
never passes through `vault-encrypt`:

```sh
export VAULT_URL=ws://<address>:4470
vault-encrypt npm-token ~/.config/vault/recipients.txt < /path/to/value
```

Avoid `echo value | …` in an interactive shell, which leaves it in your history.
Names are 1 to 64 of `A-Z a-z 0-9 . _ -`, starting with a letter or digit, and
are never overwritten: see "Rotating a secret" in the README.

## 6. The approver

Leave this running on the Mac:

```sh
vault-approve --identity ~/.config/vault/owner.identity --server "$VAULT_URL"
```

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
