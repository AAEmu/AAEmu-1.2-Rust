//! Startup check of the shared Login <-> Game `SecretKey` (Rust only).
//!
//! The shipped `Config.json` files use the secret `"test"`. That is fine on one machine, but a
//! login server whose internal listener is reachable from other machines then accepts game
//! server registrations from anyone who knows the default.

use std::net::IpAddr;

/// The secret of the shipped `Config.json` files.
pub const SHIPPED_DEFAULT_SECRET: &str = "test";
/// Secrets shorter than this many characters are reported as weak.
pub const MIN_SECRET_CHARS: usize = 16;

/// Whether `host` (a configured listen or connect address) stays on the local machine. `*`,
/// `0.0.0.0` and host names other than `localhost` are not loopback.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// The warning to log at startup, if the secret is the shipped default or shorter than
/// [`MIN_SECRET_CHARS`] while the internal link (`link_host`) is not loopback.
pub fn weak_secret_warning(secret: &str, link_host: &str) -> Option<String> {
    if is_loopback_host(link_host) {
        return None;
    }
    let reason = if secret == SHIPPED_DEFAULT_SECRET {
        "the shipped default"
    } else if secret.chars().count() < MIN_SECRET_CHARS {
        "shorter than 16 characters"
    } else {
        return None;
    };
    Some(format!(
        "SecretKey is {reason} while the internal Login <-> Game link ({link_host}) is not loopback: \
         anyone who can reach it can register as a game server or impersonate the login server. \
         Set the same random SecretKey (16+ characters) in the login and game Config.Local.json."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for host in [
            "127.0.0.1",
            "localhost",
            "LocalHost",
            "::1",
            "[::1]",
            " 127.0.0.5 ",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["*", "0.0.0.0", "10.0.0.5", "login.example.com", "", "::"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn warns_only_for_a_weak_secret_on_a_non_loopback_link() {
        assert!(weak_secret_warning("test", "0.0.0.0").is_some());
        assert!(weak_secret_warning("test", "*").is_some());
        assert!(weak_secret_warning("short-secret", "10.0.0.5").is_some());
        assert!(weak_secret_warning("", "login.internal").is_some());
        // Loopback: local development keeps the default quietly.
        assert!(weak_secret_warning("test", "127.0.0.1").is_none());
        assert!(weak_secret_warning("test", "localhost").is_none());
        // A long, non-default secret is fine anywhere.
        assert!(weak_secret_warning("0123456789abcdef", "0.0.0.0").is_none());
        assert!(weak_secret_warning("a-long-random-secret-value", "10.0.0.5").is_none());
        // The default stays weak even if it were long enough one day.
        let msg = weak_secret_warning("test", "*").unwrap();
        assert!(msg.contains("shipped default"));
    }
}
