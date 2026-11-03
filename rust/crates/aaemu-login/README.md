# aaemu-login

The AAEmu login server (Rust port of the former `AAEmu.Login` C# project).

## Running

From a source checkout (start the login server before the game server):

```sh
cd rust
cargo run --release -p aaemu-login
```

or use `Scripts/start_login.sh` / `Scripts/start AAEmu.Login.bat`. For a full local stack (MySQL in
Docker + login + game) use `cargo xtask dev`, see the
[Local Development Guide](../../../Docs/wiki/Local-Development-Guide.md).

### Application directory

`Config.json` and `Config.Local.json` are read from the *application directory*, resolved in this
order:

1. the `AAEMU_APP_PATH` environment variable;
1. the executable's directory, if it contains a `Config.json` (published layout);
1. the current directory, if it contains a `Config.json`;
1. the crate's [`app/`](app/) folder, so `cargo run` works from anywhere in the checkout.

### Logging

Logs go to stdout. The level is controlled by `RUST_LOG` (default `info`), for example
`RUST_LOG=debug` or `RUST_LOG=aaemu_login=debug`. Set `NO_COLOR=1` to disable ANSI colors.

## Setup instructions

### Configuration

The login server is configured with `Config.json` and an optional `Config.Local.json` (your local
overrides) in the application directory. Environment variables and command-line arguments are
applied on top of the files.

The configuration structure is as follows:

```
"SecretKey": "test",
"AutoAccount": true,
"InternalNetwork": {  <-- Internal network (for game server); "*" for a remote game server
    "Host": "127.0.0.1",
    "Port": 1234
},
"Network": {          <-- External network (for clients)
    "Host": "*",
    "Port": 1237,
    "NumConnections": 10
},
"Connections": {
    "MySQLProvider": {
        "Host": "%db_host%",         <-- localhost or any specific
        "Port": "%db_port%",         <-- 3306 or any specific
        "User": "%db_user%",         <-- root or any specific
        "Password": "%db_password%", <-- password
        "Database": "aaemu_login"
    }
}

```

### Two-factor authentication

The account's methods are stored in `aaemu_login.user_2fa` (`enabled_methods`: bit 1 = OTP,
2 = PC certificate, 4 = ARS). The verified methods are enforced on **every** password login —
the Trion ticket that `aaemu-launcher` uses, the Mail.Ru token and the plain password — as well as
after the Korea challenge. The prompts and answers are the 1.2 client's own packets:

| Step | Server → client | Client → server |
| --- | --- | --- |
| OTP | `ACEnterOtpPacket` 0x005 (`FUN_393d0c20`: two `i32` tries) | `CAOtpNumberPacket` 0x008 (one string, exactly 8 chars in the client, `FUN_39440c50` / `FUN_393d0a00`) |
| PC certificate | `ACEnterPcCertPacket` 0x007 (`FUN_393d0cd0`) | `CAPcCertNumberPacket` 0x00a (one string, exactly 8 chars, `FUN_39440cc0` / `FUN_393d0a60`) |
| ARS | `ACShowArsPacket` 0x006 (`FUN_393d0c60`: number + timeout ms) | phone callback (provider seam, below) |

The client's login UI listens for `OPEN_OTP` / `OPEN_PCCERT` and calls `X2.SendOtpNumber` /
`X2.SendPcCertNumber` (`ui/game/scriptsbin/x2ui/loginstage/login/login.alb`), so the prompts work
on the Trion path too.

Configuration (`TwoFactor` section; all settings optional):

```json
"TwoFactor": {
    "EnforceOnPasswordLogin": true,
    "MaxOtpFailures": 5,
    "OtpLockoutMinutes": 15
}
```

- `EnforceOnPasswordLogin` (default `true`): an account that enabled 2FA must present it after a
  correct password. `false` restores the old behaviour where only the Korea flow asked for it.
- `MaxOtpFailures` (default 5) and `OtpLockoutMinutes` (default 15): the OTP failure counter is
  **per account**, shared by all connections, so reconnecting cannot reset it; the failure that
  reaches the limit locks the account until the lockout expires. `0` disables the lock.
- A replayed TOTP code is refused: the last accepted 30-second time step is recorded per account.
  Replays are not counted as failures (someone who saw one code could otherwise lock the account).

Never log OTP or ARS codes; the server logs account ids, outcomes and masked phone numbers only.

To enrol an account, insert its Base32 TOTP secret (`OtpService::generate_secret` produces a
160-bit secret) and mark it verified:

```sql
INSERT INTO user_2fa (user_id, enabled_methods, otp_secret, otp_verified, created_at, updated_at)
VALUES (<users.id>, 1, '<BASE32_SECRET>', 1, UNIX_TIMESTAMP(), UNIX_TIMESTAMP())
ON DUPLICATE KEY UPDATE enabled_methods = 1, otp_secret = VALUES(otp_secret), otp_verified = 1;
```

The ARS phone call is a seam: `ArsPhoneProvider` (`src/services/two_factor.rs`) with a disabled
default (`DisabledArsPhoneProvider`) that places no call. An operator integration supplies the
trait implementation; the session code itself must never be logged.

### Create Configuration File

1. Create a file named `Config.Local.json` next to `Config.json` in the application directory
   (by default `rust/crates/aaemu-login/app/`). It is gitignored and overrides the default
   `Config.json` with your local changes. `ExampleConfig.Local.json` is a starting point.
1. Open `Config.Local.json` and add the configuration details as required.

For example, the `Config.Local.json` file could look like this:

```
{
    "Connections": {
        "MySQLProvider": {
            "Host": "localhost",
            "Port": "3306",
            "User": "root",
            "Password": "MySuperSecurePassword"
        }
    }
}
```

### Environment variables and command line

To keep credentials out of files entirely, set them as environment variables. A double underscore
separates the sections (`Section__Key` is `Section:Key`):

```sh
export Connections__MySQLProvider__User=root
export Connections__MySQLProvider__Password=yourpassword
```

```powershell
$env:Connections__MySQLProvider__Password = "yourpassword"
```

Command-line arguments in the form `--Section:Key=value` override everything else.

Precedence (last wins): `Config.json` < `Config.Local.json` < environment variables < command line.

> Migrating from the C# server: the login server does not read user secrets. Move any values you
> stored with `user-secrets` into `Config.Local.json` or environment variables.
