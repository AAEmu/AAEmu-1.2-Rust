# Server Downloads

- Audience: Contributors, players, and testers
- Reference revision: `e308e7461` on `main`
- Prerequisites: Rust (rustup) if building from source

Build from `main` or use a Windows or Linux archive when one is available. The
Rust project's [releases](https://github.com/AAEmu/AAEmu-1.2-Rust/releases)
page lists tagged builds. From a source checkout, run `cargo xtask publish --archive`
in `rust/` to produce the same application layout locally.

The archives contain `aaemu-login`, `aaemu-game`, default configuration, tools,
and SQL scripts. Set up MySQL, supply `compact.sqlite3` and the r208022
`game_pak`, and configure each server's `Config.Local.json`. Follow the
[published build setup](Installation-&-Setup.md#running-a-published-build).

## Related

- [Home](Home.md)
- [Installation & Setup](Installation-&-Setup.md)
- [Local Development Guide](Local-Development-Guide.md)
