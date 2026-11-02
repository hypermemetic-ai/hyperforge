# Start a ticket

One branch named after the ticket, one worktree per repo it touches, beside the checkouts:

```bash
hf workspaces worktree --name hypermemetic --ticket PLEXUS-23 --repos plexus-core,plexus-transport --dry-run true
hf workspaces worktree --name hypermemetic --ticket PLEXUS-23 --repos plexus-core,plexus-transport
```

Each branch starts from that repo's default branch (`--base` to choose). An existing branch is reused, never reset.

Then, as the work needs: [cross-repo development](cross-repo.md), [bump](bump.md), [pull requests](pr.md).

When the ticket is done and merged:

```bash
hf workspaces worktree --name hypermemetic --ticket PLEXUS-23 --repos plexus-core,plexus-transport --remove true
```

The worktrees go; the branches stay until you delete them.
