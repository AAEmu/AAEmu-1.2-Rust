//! Bearer authentication for the administrative HTTP API.

use axum::http::{header::AUTHORIZATION, HeaderMap};
use subtle::ConstantTimeEq;

/// Empty keys disable authenticated routes; malformed keys must never enable a listener.
pub(super) fn validate_key(key: Option<&str>) -> anyhow::Result<()> {
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        anyhow::ensure!(
            key.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)),
            "WebApiNetwork.ApiKey must contain only bearer-token characters"
        );
    }
    Ok(())
}

pub(super) fn authorized(key: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        return false;
    };
    // Reject duplicate credentials rather than relying on which value a proxy selects.
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Some((scheme, token)) = value.to_str().ok().and_then(|s| s.split_once(' ')) else {
        return false;
    };
    scheme.eq_ignore_ascii_case("Bearer") && bool::from(key.as_bytes().ct_eq(token.as_bytes()))
}
