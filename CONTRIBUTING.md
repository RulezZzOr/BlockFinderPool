# Contributing

Thanks for taking a look at BlockFinder.

## What to change

- Keep changes focused and small when possible.
- Prefer fixes that reduce latency, stale shares, or operator confusion.
- Avoid adding heavy dependencies or background services unless they solve a clear operational problem.

## Before you open a PR

- Run the relevant checks for the part you changed.
- Update docs when behavior or configuration changes.
- Keep dashboard labels honest: raw best, accepted best, and block-candidate paths should stay distinct.

## Local workflow

```bash
git checkout -b codex/my-change
git status -sb
git diff --check
```

For backend changes:

```bash
cd pool
cargo check
cargo test
```

For dashboard changes:

```bash
cd dashboard
npm run build
```

## Style

- Keep naming explicit.
- Prefer the existing architecture and metrics model unless you are fixing a concrete issue.
- Respect the repository license and provenance notes in `LICENSE` and `NOTICE.md`.
