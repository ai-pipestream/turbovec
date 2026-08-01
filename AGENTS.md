# AGENTS.md

## Remotes and push policy

- **This repo is GitHub-only — a deliberate exception** to the
  ai-pipestream forgejo-first rule. It is a fork of
  `RyanCodrai/turbovec` (remote `origin`), and the fork link itself is the
  collaboration channel with upstream (our `turbovec-pipestream` branch on
  the `fork` remote carries the calibration + seeded-floor patches).
  No Forgejo repo exists; do not create one without asking.
- Push our changes to remote `fork` (`github.com/ai-pipestream/turbovec`),
  never to `origin` (Ryan's repo — we have no push rights anyway).
- Workspace-wide policy and the per-repo remote table live in the
  workspace-root `../AGENTS.md` — read it before pushing anywhere.
