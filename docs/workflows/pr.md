# Pull requests

From a checkout on the branch to propose, pushed:

```bash
hf repos push_refs --path ~/dev/.../plexus-core                 # push the current branch
hf repos pr --path ~/dev/.../plexus-core --title "PLEXUS-23: ..." --body "..." --dry-run true
hf repos pr --path ~/dev/.../plexus-core --title "PLEXUS-23: ..." --body "..." --draft true
```

It targets the repo's default branch (`--base` to choose) on the forge `origin` names, using the org's credential.
