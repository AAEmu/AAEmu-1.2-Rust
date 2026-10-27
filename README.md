# ![AAEmu](https://i.imgur.com/NFDY376.png)

[![Rust](https://github.com/AAEmu/AAEmu-1.2-Rust/actions/workflows/rust.yml/badge.svg?branch=main)](https://github.com/AAEmu/AAEmu-1.2-Rust/actions/workflows/rust.yml)
[![Discord](https://img.shields.io/discord/479677351618281472?color=%235865F2&label=Discord&logo=Discord&logoColor=%23FFFFFF)](https://discord.gg/vn8E8E6)

**AAEmu 1.2 (Rust)** is an open source server emulator for **ArcheAge 1.2** (client `r208022`),
written in Rust.

It is a spin-off of [AAEmu](https://github.com/AAEmu/AAEmu), which is written in C# and covers
many client versions. Here the login server, game server and tools were ported to Rust for the 1.2
client only. The C# code is no longer in the tree, but it remains in the git history.

## Contents

- [What you need](#what-you-need)
- [Quick start (Docker)](#quick-start-docker)
- [Setup without Docker](#setup-without-docker)
- [Starting the game client](#starting-the-game-client)
- [Configuration](#configuration)
- [Development](#development)
- [Repository layout](#repository-layout)
- [Help and contributing](#help-and-contributing)
- [License](#license)

## What you need

| | Notes |
| --- | --- |
| **Rust** (stable) | Install with [rustup](https://rustup.rs/). No .NET SDK is needed. |
| **MySQL 8** | Docker runs it for you, or use one installed locally. |
| **ArcheAge 1.2 client** | The `game_pak` and `bin32/archeage.exe`. Not distributed here. |
| **`compact.sqlite3`** | The 1.2 game database. Not distributed here. |

To get the client and the database, see [Dependencies and Downloads](Docs/wiki/Dependencies-and-Downloads.md).

## Quick start (Docker)

This path needs Docker. MySQL runs in a container, and the login and game servers run on your
machine.

1. **Clone the repository:**

   ```sh
   git clone https://github.com/AAEmu/AAEmu-1.2-Rust.git
   cd AAEmu-1.2-Rust
   ```

2. **Add the game data:**
   - Copy `compact.sqlite3` to `rust/crates/aaemu-game/app/Data/`.
   - Copy the client `game_pak` to `rust/crates/aaemu-game/app/ClientData/`. Alternatively, list
     its path in `rust/crates/aaemu-game/app/Configurations/ClientData.json`.

3. **Start everything:**

   ```sh
   cd rust
   cargo xtask dev --release
   ```

   This creates `.env` from `.env.example`, starts MySQL, then starts the login server, then the
   game server.

4. **Start the client.** See [Starting the game client](#starting-the-game-client).

## Setup without Docker

1. Create the databases by importing `SQL/aaemu_login.sql` and `SQL/aaemu_game.sql` into MySQL.
2. Add the game data, as in step 2 of the Docker quick start.
3. Set the MySQL credentials for both servers. Put them in a `Config.Local.json` next to each
   `Config.json` (see [Configuration](#configuration)).
4. Start the login server first, then the game server in a second terminal:

   ```sh
   cd rust
   cargo run --release -p aaemu-login
   cargo run --release -p aaemu-game
   ```

   The scripts `Scripts/start_login.sh` / `start_game.sh` (Linux/macOS) and
   `Scripts/start AAEmu.Login.bat` / `start AAEmu.Game.bat` (Windows) do the same.

The servers apply the database updates in `SQL/updates/` at startup when
`Connections:AutoApplyUpdates` is `true`.

To run the whole stack in containers instead, `docker-compose.yaml` runs MySQL, Adminer
(http://localhost:8080), `login` and `game`. Copy `.env.example` to `.env`, run
`Scripts/docker-install-local.sh` (or `.ps1`), then `docker compose up -d`. See the
[Docker Installation Guide](Docs/wiki/Docker-Installation-Guide.md).

### Ports

| Port | Server | Used by |
| --- | --- | --- |
| 1237 | login | clients |
| 1234 | login | game server registration (listens on loopback by default) |
| 1239 | game | clients |
| 1250 | game | stream channel (UCC, emblems) |
| 1280 | game | optional web API |

To check that the stack is up:
- The login server listens on 1237.
- The game server listens on 1239 and 1250.
- The login log shows `Registered GameServer`.

## Starting the game client

The repository includes its own launcher, so no third-party launcher is needed. It starts the 1.2
client and hands it the login ticket:

```sh
cd rust
cargo run --release -p aaemu-launcher -- --client "C:\ArcheAge\bin32\archeage.exe" --login 127.0.0.1:1237 --user myname
```

- **Password:** the launcher asks for it, or takes `--password`, `--password-file` or `$AAEMU_PASSWORD`.
- **Accounts:** when `AutoAccount` is enabled, the login server creates the account on the first login.
- **Options:** `--lang` selects the client locale (default `en_us`). `--help` lists the rest.

## Configuration

Each server reads its files from its application directory:

| Server | Directory | Contents |
| --- | --- | --- |
| Login | `rust/crates/aaemu-login/app/` | `Config.json`, `ExampleConfig.Local.json` |
| Game | `rust/crates/aaemu-game/app/` | `Config.json`, `Configurations/*.json`, `Data/`, `ClientData/`, `Scripts/Commands/kits.json` |

- **Local overrides** (database password, hosts and so on) go in a `Config.Local.json` next to
  `Config.json`. This file is gitignored. Start from `ExampleConfig.Local.json`.
- **Environment variables and arguments** override the files. Environment variables use the form
  `Connections__MySQLProvider__Password=...`, command-line arguments the form `--Section:Key=value`.
- **`SecretKey`** must be the same on the login and game servers.
- **`InternalNetwork.Host`** must be set to `"*"` or an IP address in the login `Config.Local.json`
  if the game server runs on another host.
- **`AAEMU_APP_PATH`** selects another application directory. The game server also accepts the
  directory as its first argument.
- **Logging** goes to stdout and is controlled by `RUST_LOG` (default `info`).

For more detail, see:
- [Working with the Config.json files](Docs/wiki/Working-with-the-Config.json-files-and-server-listings.md)
- the [login server README](rust/crates/aaemu-login/README.md)
- the [game server README](rust/crates/aaemu-game/README.md)

## Development

```sh
cd rust
cargo build --release            # build everything
cargo test --workspace           # unit and integration tests
cargo xtask ci                   # fmt --check, clippy -D warnings, tests: exactly what CI runs
cargo xtask publish --archive    # self-contained server folders in publish/
```

- **Publishing:** `cargo xtask publish` creates `publish/aaemu-<version>-<target>/`, containing
  `AAEmu.Login/`, `AAEmu.Game/`, `Tools/` and `SQL/`. Start the binaries from their folders.
- **Live testing:** [`rust/LIVE-TESTING.md`](rust/LIVE-TESTING.md) covers running the servers
  against real data with the headless `aaemu-testclient` bot (smoke, parity and load tests).
- **Before changing game code,** read
  [`rust/crates/aaemu-game/CONTRIBUTING-PORT.md`](rust/crates/aaemu-game/CONTRIBUTING-PORT.md)
  (layout, locking rules, packet conventions).
- **Differences from the C# server** are recorded in
  [`PORTING.md`](rust/crates/aaemu-game/PORTING.md).

## Repository layout

| Path | Contents |
| --- | --- |
| `rust/` | Cargo workspace. Its crates: `aaemu-login`, `aaemu-game`, the shared `aaemu-commons`, `aaemu-physics`, `aaemu-launcher`, `aaemu-testclient`, the tools and `xtask`. |
| `SQL/` | Base MySQL schemas and the incremental updates in `SQL/updates/`. |
| `Docs/` | Documentation. [`Docs/wiki/`](Docs/wiki/Home.md) is the user guide. |
| `Scripts/` | Start, publish and Docker helper scripts. |
| `docker/`, `docker-compose.yaml` | Container images and the Compose stack. |

## Help and contributing

- **Documentation:** [Docs/wiki](Docs/wiki/Home.md), including the [FAQ](Docs/wiki/FAQ.md) and the
  [mini troubleshooting guide](Docs/wiki/Mini-troubleshoot-guide.md).
- **Questions:** the [community Discord](https://discord.gg/vn8E8E6). Please don't use the issue
  tracker for support requests.
- **Contributing:** contributions are welcome. Read the [Contributing Guidelines](CONTRIBUTING.md)
  before you open a pull request or an issue.

<a href="https://github.com/AAEmu/AAEmu-1.2-Rust/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=AAEmu/AAEmu-1.2-Rust&max=750" />
</a>

## License

	This program is free software: you can redistribute it and/or modify
	it under the terms of the GNU Lesser General Public License as published by
	the Free Software Foundation, either version 3 of the License, or
	(at your option) any later version.

	This program is distributed in the hope that it will be useful,
	but WITHOUT ANY WARRANTY; without even the implied warranty of
	MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
	GNU Lesser General Public License for more details.

	You should have received a copy of the GNU Lesser General Public License
	along with this program.  If not, see <http://www.gnu.org/licenses/>.

AAEmu is not affiliated with XLGames. All brands and trademarks belong to their respective owners.
AAEmu is not XLGames-approved software, nor is it associated with XLGames.
