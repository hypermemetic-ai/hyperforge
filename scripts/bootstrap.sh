#!/usr/bin/env bash
# hyperforge-bootstrap — zero-to-cloned in one invocation.
#
# Usage:
#   scripts/bootstrap.sh <org> [repo]
#
# Args:
#   org    GitHub org/user to pull from (required)
#   repo   Single repo name; omit to clone the entire org
#
# Env overrides:
#   HYPERFORGE_WORKSPACE   Target dir (default: ~/dev/controlflow/<org>)
#   HYPERFORGE_SRC         Hyperforge source checkout (default: ~/dev/controlflow/hypermemetic/hyperforge)
#   HYPERFORGE_TRANSPORT   ssh|https (default: https)
#
# Side effects:
#   - sudo pacman -S ... (prompts for password)
#   - Installs ghcup toolchain into ~/.ghcup/
#   - Runs `gh auth login -w` if not authenticated (opens browser, prints code)
#   - Starts hyperforge server in background (port 44104) if not running
#   - Writes GitHub token into hyperforge-auth secrets
#   - Configures gh as git credential helper (via `gh auth setup-git`)

set -euo pipefail

ORG="${1:?Usage: $0 <org> [repo]}"
REPO="${2:-}"

WORKSPACE="${HYPERFORGE_WORKSPACE:-$HOME/dev/controlflow/$ORG}"
HYPERFORGE_SRC="${HYPERFORGE_SRC:-$HOME/dev/controlflow/hypermemetic/hyperforge}"
HYPERFORGE_BIN="$HYPERFORGE_SRC/target/release/hyperforge"
TRANSPORT="${HYPERFORGE_TRANSPORT:-https}"
SERVICE_PORT=44104
AUTH_PORT=44105

log()  { printf '\033[1;36m[bootstrap]\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33m[bootstrap]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[bootstrap] error:\033[0m %s\n' "$*" >&2; exit 1; }

# --- 1. System deps via pacman ------------------------------------------------
need=()
command -v git      >/dev/null || need+=(git)
command -v gh       >/dev/null || need+=(github-cli)
command -v cargo    >/dev/null || need+=(rust)
command -v curl     >/dev/null || need+=(curl)
command -v wl-copy  >/dev/null || true  # optional

if ((${#need[@]})); then
  log "Installing pacman packages: ${need[*]} (sudo required)"
  sudo pacman -S --needed --noconfirm "${need[@]}"
fi

# --- 2. Synapse via ghcup + cabal ---------------------------------------------
export PATH="$HOME/.ghcup/bin:$HOME/.local/bin:$PATH"
if ! command -v synapse >/dev/null; then
  if [[ ! -x "$HOME/.ghcup/bin/cabal" ]]; then
    log "Installing ghcup (Haskell toolchain, ~2 GB into ~/.ghcup/)"
    curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org |
      BOOTSTRAP_HASKELL_NONINTERACTIVE=1 \
      BOOTSTRAP_HASKELL_INSTALL_NO_STACK=1 \
      BOOTSTRAP_HASKELL_INSTALL_HLS=no \
      BOOTSTRAP_HASKELL_ADJUST_BASHRC=P \
      sh
    [[ -f "$HOME/.ghcup/env" ]] && . "$HOME/.ghcup/env"
  fi
  log "Installing plexus-synapse from Hackage (~10 min cold build)"
  cabal update
  cabal install plexus-synapse \
    --installdir="$HOME/.local/bin" \
    --overwrite-policy=always
fi
command -v synapse >/dev/null || die "synapse not on PATH after install"

# --- 3. Hyperforge source + build ---------------------------------------------
if [[ ! -x "$HYPERFORGE_BIN" ]]; then
  if [[ ! -d "$HYPERFORGE_SRC/.git" ]]; then
    log "Cloning hyperforge into $HYPERFORGE_SRC"
    mkdir -p "$(dirname "$HYPERFORGE_SRC")"
    git clone https://github.com/hypermemetic/hyperforge.git "$HYPERFORGE_SRC"
  fi
  log "Building hyperforge (release, ~3 min)"
  (cd "$HYPERFORGE_SRC" && cargo build --release --bin hyperforge)
fi

# --- 4. GitHub auth ------------------------------------------------------------
if ! gh auth status -h github.com >/dev/null 2>&1; then
  warn "GitHub auth required. Browser will open; paste the code when prompted."
  warn "If the code shows, it's also copied to your clipboard (wl-copy)."
  # Start gh auth login in background so we can scrape the code for wl-copy.
  tmp=$(mktemp)
  ( gh auth login -h github.com -w -p https 2>&1 | tee "$tmp" ) &
  ghpid=$!
  # Wait up to 10s for the code line to appear, then copy to clipboard.
  for _ in $(seq 1 20); do
    code=$(grep -oE '[A-Z0-9]{4}-[A-Z0-9]{4}' "$tmp" 2>/dev/null | head -1 || true)
    [[ -n "$code" ]] && break
    sleep 0.5
  done
  if [[ -n "${code:-}" ]] && command -v wl-copy >/dev/null; then
    printf '%s' "$code" | wl-copy
    log "One-time code $code copied to clipboard."
  fi
  wait $ghpid
  rm -f "$tmp"
fi

log "Configuring git to use gh as HTTPS credential helper"
gh auth setup-git

# --- 5. Start hyperforge (if not running) -------------------------------------
if ! pgrep -f "target/release/hyperforge$" >/dev/null; then
  log "Starting hyperforge (transport=$TRANSPORT) on port $SERVICE_PORT"
  HYPERFORGE_TRANSPORT="$TRANSPORT" \
    nohup "$HYPERFORGE_BIN" >"$HOME/.cache/hyperforge.log" 2>&1 &
  disown
fi

# Wait for the WebSocket port to bind.
for _ in $(seq 1 30); do
  if ss -tln 2>/dev/null | grep -q ":${SERVICE_PORT} "; then break; fi
  sleep 1
done
ss -tln 2>/dev/null | grep -q ":${SERVICE_PORT} " || die "hyperforge did not open port $SERVICE_PORT"

# --- 6. Stash GitHub token in hyperforge-auth ---------------------------------
log "Storing GitHub token in hyperforge-auth (secrets.yaml)"
synapse -P "$AUTH_PORT" secrets auth set_secret \
  --secret_key "github/$ORG/token" \
  --value "$(gh auth token)" >/dev/null

# --- 7. Import + clone --------------------------------------------------------
log "Importing org '$ORG' from GitHub into LocalForge"
synapse -P "$SERVICE_PORT" lforge hyperforge repo import \
  --forge github --org "$ORG" >/dev/null

mkdir -p "$WORKSPACE"
if [[ -n "$REPO" ]]; then
  log "Cloning single repo $ORG/$REPO into $WORKSPACE/$REPO"
  synapse -P "$SERVICE_PORT" lforge hyperforge repo clone \
    --org "$ORG" --name "$REPO" --path "$WORKSPACE/$REPO"
else
  log "Cloning entire org $ORG into $WORKSPACE"
  synapse -P "$SERVICE_PORT" lforge hyperforge workspace clone \
    --org "$ORG" --path "$WORKSPACE"
fi

log "Done. Workspace: $WORKSPACE"
log "Server log: $HOME/.cache/hyperforge.log"
