# CLEANUP — Fix All Failing Tests Across Workspace

## Status: 7 pass, 9 fail (6 unique root causes, 3 are cascade)

## Dependency DAG

```
CLEANUP-2 (plexus-macros)  ──┐
                              ├──> CLEANUP-8 (final verification)
CLEANUP-3 (service-codegen)   ──┤
                              │
CLEANUP-4 (plexus-transport) ─┤
                              │
CLEANUP-5 (plexus-registry)  ─┤
                              │
CLEANUP-6 (plexus-substrate) ─┤
                              │
CLEANUP-7 (jsexec)         ──┘
```

All tickets are independent (no inter-dependencies). Can be done fully in parallel.

## Cascade Map

| Root failure | Cascades to |
|---|---|
| plexus-macros | synapse, plexus-deployments |
| service-codegen | plexus-sandbox-ts, synapse-cc |

Fixing 6 root causes fixes all 9 failures.

## Effort Summary

| Ticket | Repo | Difficulty | Est. Lines Changed |
|---|---|---|---|
| CLEANUP-2 | plexus-macros | Trivial | 3 |
| CLEANUP-3 | service-codegen | Moderate | 10-15 |
| CLEANUP-4 | plexus-transport | Trivial | 2 (delete) |
| CLEANUP-5 | plexus-registry | Trivial | 5 |
| CLEANUP-6 | plexus-substrate | Trivial | 1 |
| CLEANUP-7 | jsexec | Trivial | 1 |

**Total: ~25 lines across 6 repos. 5 trivial, 1 moderate. All parallelizable.**
