# Onboarding

`hf` is `synapse -P 44104 lforge-v5 hyperforge` (see the README).

1. Start the daemon (README, *Install and run*) and check `hf status`.
2. Ask hyperforge what is missing: `hf begin` walks the steps in order and says which are done.
3. Add an org with its forge token in one call:
   ```bash
   hf orgs bootstrap --name hypermemetic-ai --provider github --token gh-token:// --dry-run true
   hf orgs bootstrap --name hypermemetic-ai --provider github --token gh-token:// --import true
   ```
   `gh-token://` borrows `gh auth token`; a literal token works too (it lands in the secret store).
4. Make a workspace from the directory holding your checkouts, then check it:
   ```bash
   hf workspaces from_org --help        # build one from an org's repos
   hf workspaces reconcile --name hypermemetic --dry-run true
   ```
   Read the reconcile plan before running it without `--dry-run`: it rewrites the workspace's member list.
5. Collect registry tokens: `scripts/auth-walk.py --dry-run`, then without it. See [secrets](secrets.md).
6. `hf auth_check` proves each forge credential works; `hf auth_requirements` lists what each org still needs.
