#!/bin/bash
# container-session.sh - Load hyperforge-lforge2 and dependencies into claude-container
# with optional session import

# Don't use set -e so we can capture and display errors
# set -e

CLAUDE_CONTAINER=~/dev/controlflow/juggernautlabs/claude-container/claude-container
CLAUDE_CONTAINER_CP=~/dev/controlflow/juggernautlabs/claude-container/claude-container-cp
SESSION_NAME="${1:-hyperforge-lforge2}"
CURRENT_SESSION_DIR=~/.claude/projects/-Users-user-dev-controlflow-hypermemetic-hyperforge-lforge2
CURRENT_SESSION_ID="${2:-5534791e-38d7-4478-b668-05b40ff8215f}"

echo "=== Hyperforge Container Session Setup ==="
echo "Session name: $SESSION_NAME"
echo "Current session ID: $CURRENT_SESSION_ID"
echo ""

# Check if claude-container exists
if [[ ! -x "$CLAUDE_CONTAINER" ]]; then
    echo "Error: claude-container not found at $CLAUDE_CONTAINER"
    exit 1
fi

# Check if Docker is running
if ! docker info &>/dev/null; then
    echo "Error: Docker is not running."
    echo "Start it with: colima start"
    exit 1
fi
echo "Docker: OK"

# Step 1: Create the git session without running (--no-run)
echo "Step 1: Creating git session with dependencies..."
cd ~/dev/controlflow/hypermemetic/hyperforge-lforge2

if ! "$CLAUDE_CONTAINER" --git-session "$SESSION_NAME" --no-run; then
    echo ""
    echo "Error: Failed to create git session. See output above."
    echo "Make sure Docker/Colima is running: colima start"
    exit 1
fi

# Step 2: Start container briefly to create volumes, then copy session data
if [[ -f "$CURRENT_SESSION_DIR/$CURRENT_SESSION_ID.jsonl" ]]; then
    echo ""
    echo "Step 2: Creating state volume and importing session..."

    # Container paths
    CONTAINER_PROJECT_DIR="/home/developer/.claude/projects/-workspace-hyperforge"
    CONTAINER_TRANSCRIPT="$CONTAINER_PROJECT_DIR/$CURRENT_SESSION_ID.jsonl"

    # Create state volume and project directory
    echo "  Creating state volume..."
    docker run --rm \
        -v "claude-state-${SESSION_NAME}:/home/developer/.claude" \
        ghcr.io/hypermemetic/claude-container:latest \
        mkdir -p "$CONTAINER_PROJECT_DIR" 2>/dev/null || true

    # Copy the session transcript
    echo "  Copying transcript: $CURRENT_SESSION_ID.jsonl"
    if "$CLAUDE_CONTAINER_CP" "$CURRENT_SESSION_DIR/$CURRENT_SESSION_ID.jsonl" \
        "$SESSION_NAME:$CONTAINER_TRANSCRIPT" 2>/dev/null; then
        echo "  ✓ Transcript copied"
    else
        echo "  ✗ Failed to copy transcript"
    fi

    # Rewrite host paths to container paths in the transcript
    HOST_PATH="/Users/user/dev/controlflow/hypermemetic/hyperforge-lforge2"
    CONTAINER_PATH="/workspace/hyperforge"
    echo "  Rewriting paths: $HOST_PATH -> $CONTAINER_PATH"
    docker run --rm \
        -v "claude-state-${SESSION_NAME}:/home/developer/.claude" \
        ghcr.io/hypermemetic/claude-container:latest \
        sed -i "s|$HOST_PATH|$CONTAINER_PATH|g" "$CONTAINER_TRANSCRIPT" 2>/dev/null && \
        echo "  ✓ Paths rewritten" || echo "  ✗ Failed to rewrite paths"

    # Generate sessions-index.json with proper container paths
    echo "  Generating sessions-index.json..."
    TEMP_INDEX=$(mktemp)
    NOW=$(date -u +"%Y-%m-%dT%H:%M:%S.000Z")
    MTIME=$(date +%s)000

    cat > "$TEMP_INDEX" << EOF
{
  "version": 1,
  "entries": [
    {
      "sessionId": "$CURRENT_SESSION_ID",
      "fullPath": "$CONTAINER_TRANSCRIPT",
      "fileMtime": $MTIME,
      "firstPrompt": "Imported session from host",
      "summary": "Hyperforge LFORGE2 Development Session",
      "messageCount": 100,
      "created": "$NOW",
      "modified": "$NOW",
      "gitBranch": "feat/lforge2-redesign",
      "projectPath": "/workspace/hyperforge",
      "isSidechain": false
    }
  ],
  "originalPath": "/workspace/hyperforge"
}
EOF

    if "$CLAUDE_CONTAINER_CP" "$TEMP_INDEX" \
        "$SESSION_NAME:$CONTAINER_PROJECT_DIR/sessions-index.json" 2>/dev/null; then
        echo "  ✓ sessions-index.json created"
    else
        echo "  ✗ Failed to create sessions-index.json"
    fi
    rm -f "$TEMP_INDEX"

    echo "  Session data imported!"
else
    echo ""
    echo "Step 2: Skipping session import (transcript not found)"
fi

echo ""
echo "=== Setup Complete ==="
echo ""
echo "To start the container:"
echo "  $CLAUDE_CONTAINER --git-session $SESSION_NAME --continue"
echo ""
echo "Or without continuing previous conversation:"
echo "  $CLAUDE_CONTAINER --git-session $SESSION_NAME"
echo ""
echo "Inside container, workspace layout:"
echo "  /workspace/hyperforge/          (main project, on feat/lforge2-redesign branch)"
echo "  /workspace/plexus-core/            (dependency)"
echo "  /workspace/plexus-macros/           (dependency)"
echo "  /workspace/plexus-transport/       (dependency)"
