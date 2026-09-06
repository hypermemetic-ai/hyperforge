#!/usr/bin/env python3
"""auth-walk — walk every credential hyperforge needs, one site at a time.

    scripts/auth-walk.py                 # everything that is missing
    scripts/auth-walk.py --only cargo/token --force
    scripts/auth-walk.py --dry-run       # just the list, no browser, no store

For each credential that is missing (or every one, with --force) the walk
prints what the credential is for and which scopes it needs, opens the
provider's token page in your browser, and then WAITS: it watches the
clipboard until it changes to something shaped like that provider's
token. It validates the token against the provider (a read that needs
auth, or hyperforge's own idiom — an authenticated call on a target that
does not exist, where "not found" proves the token and "not logged in"
disproves it), stores it through hyperforge's `secrets set` RPC, clears
the clipboard, and moves to the next site. The token is never printed.

Which credentials? Two sources, in this order:

  1. hyperforge itself — `auth_requirements`: one row per org × forge
     credential ref, with `present`. This is the authoritative list of
     forge tokens.
  2. the registry channels hyperforge publishes through — the build hub
     resolves `cargo/token`, `npm/token`, `pypi/token`, and the Hackage
     publisher `hackage/token`. `auth_requirements` does not report these
     yet (V5PARITY-37); until it does, the table below is the second
     source, checked against `secrets list_refs`.

Needs a hyperforge daemon (spawned on demand if the port is closed) and a
`synapse` that runs: $SYNAPSE, then PATH, then the newest build under the
synapse checkout. Clipboard: wl-paste/wl-copy (Wayland), xclip, or pbpaste.
"""
from __future__ import annotations

import argparse
import atexit
import glob
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

HOME = os.path.expanduser("~")
CONFIG_DIR = os.environ.get("HYPERFORGE_CONFIG_DIR", f"{HOME}/.config/hyperforge")
UA = {"User-Agent": "hyperforge-auth-walk"}


# ---------------------------------------------------------------------
# The catalogue: what each credential is, where it comes from, what it
# looks like, and how to prove it works without changing anything.

def _get(url: str, headers: dict, method: str = "GET"):
    """(status, body) — never raises on HTTP errors."""
    req = urllib.request.Request(url, headers={**UA, **headers}, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:  # network
        return 0, str(e)


def v_github(tok):
    st, body = _get("https://api.github.com/user", {"Authorization": f"Bearer {tok}"})
    return (st == 200, json.loads(body).get("login") if st == 200 else f"HTTP {st}")


def v_codeberg(tok):
    st, body = _get("https://codeberg.org/api/v1/user", {"Authorization": f"token {tok}"})
    if st == 200:
        return True, json.loads(body).get("login")
    # A token without read:user still authenticates elsewhere.
    st2, _ = _get("https://codeberg.org/api/v1/repos/search?limit=1", {"Authorization": f"token {tok}"})
    return (st2 == 200, f"HTTP {st}/{st2}")


def v_gitlab(tok):
    st, body = _get("https://gitlab.com/api/v4/user", {"PRIVATE-TOKEN": tok})
    return (st == 200, json.loads(body).get("username") if st == 200 else f"HTTP {st}")


def v_crates(tok):
    # crates.io keeps /me website-only, so: yank a crate that does not
    # exist. A bad token is refused before the crate is looked up (403,
    # "must be logged in"); a good one reaches the lookup (404).
    st, body = _get(
        "https://crates.io/api/v1/crates/hyperforge-auth-probe-nonexistent/0.0.0/yank",
        {"Authorization": tok}, method="DELETE",
    )
    if st == 404:
        return True, "authenticated (probe 404)"
    if st == 403 and "logged in" not in body and "token" not in body.lower():
        return True, "authenticated (probe 403: " + body[:60] + ")"
    return False, f"HTTP {st} {body[:80]}"


def v_npm(tok):
    st, body = _get("https://registry.npmjs.org/-/whoami", {"Authorization": f"Bearer {tok}"})
    return (st == 200, json.loads(body).get("username") if st == 200 else f"HTTP {st}")


def v_hackage(tok):
    st, _ = _get("https://hackage.haskell.org/users/account-management", {"Authorization": f"X-ApiKey {tok}"})
    return (st == 200, f"HTTP {st}")


def v_none(_tok):
    return True, "no validator for this provider — stored unverified"


CATALOG = {
    # forge tokens (keys come from auth_requirements; the provider picks the row)
    "github": dict(
        name="GitHub personal access token",
        url="https://github.com/settings/tokens/new?scopes=repo,read:org,workflow,delete_repo",
        scopes="repo · read:org · workflow (· delete_repo if hyperforge purges here)",
        shape=r"^(ghp_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})$",
        validate=v_github,
    ),
    "codeberg": dict(
        name="Codeberg access token",
        url="https://codeberg.org/user/settings/applications",
        scopes="repository read+write · organization read+write · user read",
        shape=r"^[0-9a-f]{40}$",
        validate=v_codeberg,
    ),
    "gitlab": dict(
        name="GitLab personal access token",
        url="https://gitlab.com/-/user_settings/personal_access_tokens",
        scopes="api · read_api · write_repository",
        shape=r"^glpat-[A-Za-z0-9_\-]{20,}$",
        validate=v_gitlab,
    ),
    # registry tokens (keys are fixed; see the docstring)
    "cargo/token": dict(
        name="crates.io API token",
        url="https://crates.io/settings/tokens/new",
        scopes="publish-new · publish-update · yank",
        shape=r"^cio[A-Za-z0-9]{29,60}$",
        validate=v_crates,
        after="cargo login",
    ),
    "npm/token": dict(
        name="npm access token",
        url="https://www.npmjs.com/settings/~/tokens/new",
        scopes="granular: read+write packages you publish",
        shape=r"^npm_[A-Za-z0-9]{30,}$",
        validate=v_npm,
    ),
    "hackage/token": dict(
        name="Hackage API key",
        url="https://hackage.haskell.org/users/account-management",
        scopes="an API key from account management (used as --token by cabal upload)",
        shape=r"^\S{16,}$",
        validate=v_hackage,
    ),
    "pypi/token": dict(
        name="PyPI API token",
        url="https://pypi.org/manage/account/token/",
        scopes="scoped to the project, or account-wide",
        shape=r"^pypi-[A-Za-z0-9_\-]{40,}$",
        validate=v_none,
    ),
}
REGISTRY_KEYS = ["cargo/token", "npm/token", "hackage/token", "pypi/token"]


# ---------------------------------------------------------------------
# hyperforge: the daemon and the RPC

def find_synapse(explicit: str | None) -> str:
    cands = [explicit, os.environ.get("SYNAPSE"), shutil.which("synapse")]
    for c in cands:
        if c and os.path.exists(os.path.realpath(c)):
            return c
    built = glob.glob(f"{HOME}/dev/controlflow/hypermemetic/synapse/dist-newstyle/build/*/ghc-*/plexus-synapse-*/x/synapse/build/synapse/synapse")
    if built:
        return max(built, key=os.path.getmtime)
    sys.exit("no runnable synapse: set $SYNAPSE (the one on PATH is a dangling symlink?)")


def port_open(port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(0.3)
        return s.connect_ex(("127.0.0.1", port)) == 0


def ensure_daemon(port: int):
    if port_open(port):
        return
    exe = shutil.which("hyperforge") or sys.exit("hyperforge not on PATH and port closed")
    proc = subprocess.Popen([exe, "--port", str(port), "--config-dir", CONFIG_DIR],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    atexit.register(proc.terminate)
    for _ in range(50):
        if port_open(port):
            return
        time.sleep(0.1)
    sys.exit("hyperforge daemon did not open its port")


class Hf:
    def __init__(self, synapse: str, port: int):
        self.synapse, self.port = synapse, port

    def call(self, *args: str) -> list[dict]:
        out = subprocess.run([self.synapse, "-P", str(self.port), "--json", "lforge-v5", "hyperforge", *args],
                             capture_output=True, text=True, timeout=120)
        events = []
        for line in out.stdout.splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            d = json.loads(line)
            c = d.get("content", d)
            if c.get("type") != "done":
                events.append(c)
        if out.returncode != 0 and not events:
            sys.exit(f"synapse {' '.join(args[:2])} failed: {out.stderr.strip()[:200]}")
        return events

    def requirements(self) -> list[dict]:
        """Every credential hyperforge needs, with `present`."""
        rows, seen = [], {}
        for ev in self.call("auth_requirements"):
            if ev.get("type") != "auth_requirement" or ev.get("cred_type") != "token":
                continue
            key = ev["key"].removeprefix("secrets://")
            r = seen.get(key)
            if r is None:
                r = seen[key] = dict(key=key, provider=ev["provider"], orgs=[], present=bool(ev.get("present")))
                rows.append(r)
            r["orgs"].append(ev["org"])
        present = {e["key"].removeprefix("secrets://") for e in self.call("secrets", "list_refs") if e.get("type") == "secret_ref"}
        for key in REGISTRY_KEYS:
            rows.append(dict(key=key, provider=key, orgs=["build.publish"], present=key in present))
        return rows

    def store(self, key: str, value: str) -> bool:
        evs = self.call("secrets", "set", "--key", f"secrets://{key}", "--value", value)
        return any(e.get("type") == "secret_set" for e in evs)


# ---------------------------------------------------------------------
# clipboard

CLEARED = "hyperforge: token stored, clipboard cleared"


def clipboard_cmds():
    """(paste command, copy command that takes the text on stdin)."""
    if shutil.which("wl-paste"):
        return ["wl-paste", "-n"], ["wl-copy", "-n"]
    if shutil.which("xclip"):
        return ["xclip", "-o", "-selection", "clipboard"], ["xclip", "-selection", "clipboard", "-i"]
    if shutil.which("pbpaste"):
        return ["pbpaste"], ["pbcopy"]
    sys.exit("no clipboard tool (wl-paste, xclip, or pbpaste)")


def read_clip(cmd) -> str:
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=5).stdout.strip()
    except Exception:
        return ""


def clear_clip(copy_cmd, paste_cmd, token: str) -> bool:
    """Overwrite the clipboard with a marker and read it back. A bare
    `--clear` is not enough: clipboard managers (Klipper's "prevent empty
    clipboard", and the like) put the last entry straight back, so the
    token would survive. Overwriting sticks. True if the token is gone."""
    # wl-copy (and xclip) fork a child that keeps serving the selection
    # until something replaces it, and that child inherits our pipes — so
    # never capture its output, or this call blocks for as long as the
    # marker stays on the clipboard.
    try:
        subprocess.run(copy_cmd, input=CLEARED, text=True, timeout=5,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except Exception:
        return False
    for _ in range(8):
        time.sleep(0.25)
        if read_clip(paste_cmd) != token:
            return True
    return False


def wait_for_token(paste_cmd, before: str, shape: str, timeout: float) -> str | None:
    pat = re.compile(shape)
    warned = False
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        now = read_clip(paste_cmd)
        if now and now != before:
            if pat.match(now):
                return now
            if not warned:
                print("    clipboard changed, but that does not look like this token — still waiting")
                warned = True
                before = now
        time.sleep(0.5)
    return None


# ---------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", action="append", metavar="KEY", help="walk only this key (repeatable), e.g. cargo/token or codeberg/_default/token")
    ap.add_argument("--force", action="store_true", help="re-collect credentials that are already present")
    ap.add_argument("--dry-run", action="store_true", help="list the walk and stop: no browser, no wait, no store")
    ap.add_argument("--no-open", action="store_true", help="print the URL instead of opening the browser")
    ap.add_argument("--keep-clipboard", action="store_true", help="do not clear the clipboard after storing")
    ap.add_argument("--no-cargo-login", action="store_true", help="for cargo/token: skip the extra `cargo login`")
    ap.add_argument("--timeout", type=float, default=600, help="seconds to wait per token (default 600)")
    ap.add_argument("--port", type=int, default=44104)
    ap.add_argument("--synapse", help="path to a runnable synapse")
    a = ap.parse_args()

    synapse = find_synapse(a.synapse)
    ensure_daemon(a.port)
    hf = Hf(synapse, a.port)
    paste_cmd, copy_cmd = clipboard_cmds()

    rows = hf.requirements()
    if a.only:
        rows = [r for r in rows if r["key"] in a.only]
        missing = set(a.only) - {r["key"] for r in rows}
        if missing:
            sys.exit(f"unknown key(s): {', '.join(sorted(missing))}")
    todo = [r for r in rows if a.force or not r["present"]]

    print(f"hyperforge needs {len(rows)} credential(s); {len(todo)} to collect" + (" (dry run)" if a.dry_run else ""))
    for r in rows:
        mark = "·" if r["present"] and r not in todo else "→"
        print(f"  {mark} {r['key']:32} {'present' if r['present'] else 'missing':8} for {', '.join(r['orgs'])}")
    if a.dry_run or not todo:
        return

    results = []
    for r in todo:
        spec = CATALOG.get(r["provider"]) or CATALOG.get(r["key"])
        if spec is None:
            print(f"\n{r['key']}: no catalogue entry — skipped")
            results.append((r["key"], "skipped"))
            continue
        print(f"\n== {spec['name']}  →  secrets://{r['key']}")
        print(f"   for: {', '.join(r['orgs'])}")
        print(f"   scopes: {spec['scopes']}")
        before = read_clip(paste_cmd)
        if a.no_open or not shutil.which("xdg-open") and not shutil.which("open"):
            print(f"   open: {spec['url']}")
        else:
            opener = shutil.which("xdg-open") or shutil.which("open")
            subprocess.Popen([opener, spec["url"]], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            print(f"   opened {spec['url']}")
        print("   copy the token; waiting on the clipboard …")
        tok = wait_for_token(paste_cmd, before, spec["shape"], a.timeout)
        if tok is None:
            print("   timed out — skipped")
            results.append((r["key"], "timed out"))
            continue
        ok, detail = spec["validate"](tok)
        if not ok:
            print(f"   rejected by the provider: {detail} — not stored")
            results.append((r["key"], f"rejected: {detail}"))
            continue
        print(f"   validated: {detail}")
        if not hf.store(r["key"], tok):
            print("   hyperforge did not confirm the store — check the daemon")
            results.append((r["key"], "store failed"))
            continue
        print(f"   stored secrets://{r['key']}")
        if spec.get("after") == "cargo login" and not a.no_cargo_login and shutil.which("cargo"):
            subprocess.run(["cargo", "login"], input=tok + "\n", capture_output=True, text=True)
            print("   cargo login done (~/.cargo/credentials.toml)")
        if not a.keep_clipboard:
            if clear_clip(copy_cmd, paste_cmd, tok):
                print("   clipboard cleared (overwritten with a marker; a clipboard manager may still hold it in history)")
            else:
                print("   could not clear the clipboard — the token is still on it")
        results.append((r["key"], "stored"))

    print("\nsummary")
    for key, res in results:
        print(f"  {key:32} {res}")


if __name__ == "__main__":
    main()
