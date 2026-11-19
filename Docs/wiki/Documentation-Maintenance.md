# Documentation Maintenance

- Audience: Contributors
- Reference revision: `e308e7461` on `main`
- Prerequisites: None

## Goal

Keep the guides under `Docs/wiki/` aligned with merged behavior in `main`.
The repository is the source for these guides.

## Update checklist after merged PRs

1. Identify pages impacted by launch, config, networking, packaging, or data
   behavior changes.
1. Update affected pages and cross-links.
1. Add or adjust migration notes when old instructions become invalid.
1. Follow changed links and check commands against script help, configuration
   against the shipped defaults and loaders, and feature descriptions against
   current tests. Label historical audit results with their reference revision.
1. Commit following the contributor guidelines.

## Writing conventions

- Use page metadata at top (`Audience`, `Last verified against`).
- Link to the `.md` file for neighboring pages. The optional manual wiki
  workflow converts these links to wiki page names when publishing.
- Add a short `Related` section on major pages.
- Keep instructions path-based and explicit.
- Prefer `Config.Local.json` examples for machine-specific values.

## Cross-linking model

1. `Home` is the primary table of contents.
1. Setup pages link to config and troubleshooting pages.
1. Troubleshooting pages link back to setup and FAQ.
1. Reference pages link to setup where relevant.

## Related

- [Home](Home.md)
- [Developer Notes](Developer-Notes.md)
- [Local Development Guide](Local-Development-Guide.md)
