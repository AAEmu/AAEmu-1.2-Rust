# ![AAEmu](https://i.imgur.com/NFDY376.png)

Open source server software for ArcheAge written in Rust (originally written in C#/.NET; the C# code has been removed).

## Start Here

- Audience: Contributors, players, and testers
- Reference revision: `e308e7461` on `main`
- Prerequisites: None
- Preferred local development: [Local Development Guide](Local-Development-Guide.md)
- Manual setup fallback: [Installation & Setup](Installation-&-Setup.md)
- Docker setup alternative: [Docker Installation Guide](Docker-Installation-Guide.md)
- Troubleshooting first stop: [Mini troubleshoot guide](Mini-troubleshoot-guide.md)
- Need help: [Getting Help](Getting-Help.md)

## Documentation Map

### Getting Started

- [Dependencies and Downloads](Dependencies-and-Downloads.md)
- [Local Development Guide](Local-Development-Guide.md)
- [Installation & Setup](Installation-&-Setup.md)
- [Docker Installation Guide](Docker-Installation-Guide.md)

### Configuration

- [Working with the Config.json files and server listings](Working-with-the-Config.json-files-and-server-listings.md)
- [Test server content (Marketplace catalogue and GM kits)](Test-Server-Content.md)

### Operations and Troubleshooting

- [Mini troubleshoot guide](Mini-troubleshoot-guide.md)
- [FAQ](FAQ.md)

### Help and Support

- [Getting Help](Getting-Help.md)
- [Asking for Help on Discord](Asking-for-Help-on-Discord.md)
- [Asking for Help on GitHub Discussions](Asking-for-Help-on-GitHub-Discussions.md)
- [Help Us Help You](Help-Us-Help-You.md)

### Project Reference

- [Components](Components.md)
- [Code Terminology](Code-Terminology.md)
- [Developer Notes](Developer-Notes.md)
- [GM Commands](GM-Commands.md)
- [Dungeons and Mentoring](Dungeons.md)
- [Live Verification Suites](Verification.md)
- [Quest Technical Audit](Quest-Audit.md)
- [Mail Technical Audit](Mail-Audit.md)
- [Client](Client.md)
- [Server](Server.md)

### Legacy Community Guides

- [Alternative Guide](https://docs.google.com/document/d/1XfZR6zb9-n2oldu1NB9e5_9eLEwam8r2wAt3vKA5Vpo/edit)
- [Alternative Guide (Russian/на русском языке)](https://docs.google.com/document/d/1O_v6dTyvv99tBgjvoGXUReKUj5azmzn7mdAD4FsUfKM/edit?usp=sharing)

### Contributing Docs

- [Documentation Maintenance](Documentation-Maintenance.md)

## Current Platform

- The servers have been ported from C# (.NET) to Rust. The Cargo workspace is
  in `rust/`; build with `cargo build --release`.
- The former C# projects have been removed from the repository; they remain
  only in the git history.
- `cargo xtask dev` (MySQL in Docker + login + game) is the preferred
  contributor workflow.
- The client is started with AAEmu's own `aaemu-launcher`; no third-party
  launcher is needed (see [Client](Client.md)).
- Developer tooling: `aaemu-clientdriver` (Lua driver for the real client) and
  the CryPhysics port `aaemu-physics` (zone collision, configured in
  `Configurations/Physics.json`).
- Runtime configuration and data live in the crates' application directories:
  `rust/crates/aaemu-login/app/` and `rust/crates/aaemu-game/app/`.
- Both servers write console output and files under their application directory's
  `Logs/` folder. Use `RUST_LOG` to change the log filter.
- Login server game server listings are configured via `GameServers`
  configuration (not MySQL `game_servers`).
- `Config.Local.json` overrides the shipped game configuration files.
  User secrets, environment variables, and command-line arguments have higher precedence.

## Related

- [Local Development Guide](Local-Development-Guide.md)
- [Dependencies and Downloads](Dependencies-and-Downloads.md)
- [Installation & Setup](Installation-&-Setup.md)
- [FAQ](FAQ.md)
