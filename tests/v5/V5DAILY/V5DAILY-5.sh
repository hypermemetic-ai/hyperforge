#!/usr/bin/env bash
# tier: 1
# repos pr against a fake GitHub: refuses what is unpushed, opens what is.
set -euo pipefail
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
export HF_GITHUB_API_URL=http://127.0.0.1:$PORT
source "$(dirname "$0")/../harness/lib.sh"
hf_spawn
TMP="$(mktemp -d -t v5daily-XXXXXX)"
cat > "$TMP/fake.py" <<'PY'
import json,sys
from http.server import BaseHTTPRequestHandler as B, HTTPServer
class H(B):
    def log_message(s,*a): pass
    def out(s,c,o):
        b=json.dumps(o).encode(); s.send_response(c); s.send_header("Content-Length",str(len(b))); s.end_headers(); s.wfile.write(b)
    def do_GET(s):
        s.out(200,{"default_branch":"main"})
    def do_POST(s):
        d=json.loads(s.rfile.read(int(s.headers["Content-Length"])))
        ok=s.headers.get("Authorization")=="Bearer tok" and d["head"]=="feat" and d["base"]=="main" and s.path=="/repos/demo/alpha/pulls"
        s.out(201 if ok else 422,{"number":7,"html_url":"https://x/pull/7"} if ok else {"message":"bad"})
HTTPServer(("127.0.0.1",int(sys.argv[1])),H).serve_forever()
PY
python3 -I "$TMP/fake.py" "$PORT" & FAKE=$!
trap 'kill $FAKE; hf_teardown; rm -rf "$TMP"' EXIT
sleep 0.5
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
git init -q --bare "$TMP/up/demo/alpha.git"
git init -q -b main "$TMP/alpha"
git -C "$TMP/alpha" commit -q --allow-empty -m seed
git -C "$TMP/alpha" remote add origin "$TMP/up/demo/alpha.git"
git -C "$TMP/alpha" push -qu origin main
git -C "$TMP/alpha" remote set-url origin https://github.com/demo/alpha.git
git -C "$TMP/alpha" remote set-url --push origin "$TMP/up/demo/alpha.git"
git -C "$TMP/alpha" checkout -q -b feat
git -C "$TMP/alpha" commit -q --allow-empty -m work
hf_cmd secrets set --key secrets://github/demo/token --value tok >/dev/null
hf_cmd repos pr --path "$TMP/alpha" --title t | hf_assert_event '.type == "error" and .code == "unpushed"'
git -C "$TMP/alpha" push -qu origin feat
git -C "$TMP/alpha" commit -q --allow-empty -m more
hf_cmd repos pr --path "$TMP/alpha" --title t | hf_assert_event '.type == "error" and .code == "unpushed"'
git -C "$TMP/alpha" push -q origin feat
hf_cmd repos pr --path "$TMP/alpha" --title t --dry-run true | hf_assert_event '.type == "pull_request" and .dry_run == true and .base == "main"'
hf_cmd repos pr --path "$TMP/alpha" --title t | hf_assert_event '.type == "pull_request" and .number == 7 and .url == "https://x/pull/7"'
echo PASS
