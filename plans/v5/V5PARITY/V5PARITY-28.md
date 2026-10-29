---
id: V5PARITY-28
title: "ASSETS — release artifact upload to forge releases"
status: Pending
type: implementation
blocked_by: []
unlocks: []
---

## Problem

v4 had `repos.assets` (list release artifacts) and `repos.upload` (push artifacts to a release). v5 has `build.release` (creates the tag + github release) but no way to attach binaries / archives to that release. Workflows that ship pre-built binaries via GitHub Releases cannot use v5 today.

## Required behavior

**`ForgePort` trait additions** — adapter-implemented:
- `list_release_assets(repo_ref, tag, auth)` → `Vec<ReleaseAsset { name, size, download_url }>`
- `upload_release_asset(repo_ref, tag, name, content_type, bytes, auth)` → `ReleaseAsset`
- `delete_release_asset(repo_ref, asset_id, auth)` (rename via delete-then-upload; GitHub asset names are immutable post-upload).

**`ReleaseAsset`** is the closed type pinned in CONTRACTS §types: `{ name: String, size: u64, download_url: String, content_type: String }`. v1 implements GitHub; codeberg/gitlab return `Unimplemented` until adapters land.

**RPC methods on `ReposService`:**

| Method | Behavior |
|---|---|
| `repos.list_assets --org X --name N --tag T` | Streams `release_asset` events. |
| `repos.upload_asset --org X --name N --tag T --file P [--content_type CT]` | Reads file bytes, calls adapter. Emits `asset_uploaded { name, size, download_url }`. Errors typed (`asset_too_large`, `tag_not_found`, `auth_required`). |
| `repos.delete_asset --org X --name N --tag T --name N` | Idempotent — `not_found` is silent success. |

**`build.release` integration:** new optional `--assets <glob>` param. If present, `release` runs the existing flow (bump → push → forge release → publish), then uploads every file matching the glob (relative to the repo root) to the new release. Common case: `--assets "target/release/*-linux-x86_64.tar.gz"`.

## What must NOT change

- D13 — adapter calls go through `ops::repo::*` wrappers; `ReposService` doesn't talk to adapters directly.
- D9 secret redaction — asset content is binary; `bytes` never appears in event payloads.
- Existing `build.release` semantics (tag + push + forge release creation) stay byte-identical when `--assets` is absent.

## Acceptance criteria

1. `repos.upload_asset --org X --name N --tag v0.1.0 --file ./build/foo.tar.gz` against a tag created by `build.release` succeeds and the asset is downloadable from the github release page.
2. `repos.list_assets` after the upload streams one `release_asset` event with the expected name + size.
3. Re-uploading the same name returns `asset_exists`; with `--replace true`, deletes + re-uploads.
4. `build.release … --assets "target/release/*-linux-x86_64.tar.gz"` after compiling a binary produces a release with the matching artifacts attached.
5. Tier-2 — needs a real GitHub repo + token with `repo` scope. Tier-1 stub against `httpmock` covers the URL-shape contract.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-28.sh` → exit 0.
- Ready → Complete in-commit.
