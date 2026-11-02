# Orient

Where every checkout stands, worktrees included:

```bash
hf workspaces orient --name hypermemetic --table true             # branch, dirty, ahead/behind, upstream
hf workspaces orient --name hypermemetic --table true --dirty-only true
hf workspaces status --name hypermemetic                          # one event per member
hf build package_diff --name hypermemetic --include 'plexus-*'    # each crate against its registry
```

`package_diff` says `unpublished`, `ahead` (a version the registry lacks), `published`, `drifted` (published, but the files changed since: bump it), `behind` or `yanked`.

Keep the workspace in step with the disk: `hf workspaces reconcile --name hypermemetic --dry-run true` lists repos added, moved or gone.
