//! The channel registry: `channel -> handler`, plus the feature names the server supports.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::net::connection::GameConnection;
use crate::server::GameServer;

use super::packets::valid_channel;

/// A channel handler. `Err(code)` is sent back as `ext.error { ok: false, error: code }`.
pub type ChannelHandler =
    Box<dyn Fn(&GameServer, &mut GameConnection, Value) -> Result<(), &'static str> + Send + Sync>;

/// Called with the connection id when a connection closes.
pub type DisconnectHook = fn(&GameServer, u32);

/// Filled by [`super::System::default`]; read-only afterwards.
#[derive(Default)]
pub struct ExtRegistry {
    handlers: HashMap<&'static str, ChannelHandler>,
    features: Vec<&'static str>,
    disconnect: Vec<DisconnectHook>,
}

impl std::fmt::Debug for ExtRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtRegistry")
            .field("channels", &self.handlers.len())
            .field("features", &self.features)
            .finish()
    }
}

impl ExtRegistry {
    /// Declares a feature name that `ext.welcome` may list.
    pub fn feature(&mut self, name: &'static str) -> &mut Self {
        if !self.features.contains(&name) {
            self.features.push(name);
        }
        self
    }

    /// Registers the handler of a channel. The handler gets the server, the connection and the
    /// decoded JSON object. Panics on an invalid or duplicate channel name (a coding error).
    pub fn on(
        &mut self,
        channel: &'static str,
        handler: impl Fn(&GameServer, &mut GameConnection, Value) -> Result<(), &'static str>
            + Send
            + Sync
            + 'static,
    ) -> &mut Self {
        assert!(valid_channel(channel), "invalid ext channel {channel:?}");
        assert!(
            !channel.starts_with("ext."),
            "ext.* channels are reserved: {channel}"
        );
        let old = self.handlers.insert(channel, Box::new(handler));
        assert!(old.is_none(), "ext channel {channel} registered twice");
        self
    }

    /// [`ExtRegistry::on`] with the payload deserialized into `T` (`bad_payload` on failure).
    pub fn on_typed<T: DeserializeOwned + 'static>(
        &mut self,
        channel: &'static str,
        handler: fn(&GameServer, &mut GameConnection, T),
    ) -> &mut Self {
        self.on(
            channel,
            move |server, conn, value| match serde_json::from_value::<T>(value) {
                Ok(t) => {
                    handler(server, conn, t);
                    Ok(())
                }
                Err(e) => {
                    tracing::debug!("ext {channel}: bad payload: {e}");
                    Err("bad_payload")
                }
            },
        )
    }

    /// Registers a connection-closed hook.
    pub fn on_disconnect(&mut self, hook: DisconnectHook) -> &mut Self {
        self.disconnect.push(hook);
        self
    }

    pub fn handler(&self, channel: &str) -> Option<&ChannelHandler> {
        self.handlers.get(channel)
    }

    pub fn features(&self) -> &[&'static str] {
        &self.features
    }

    pub fn channel_count(&self) -> usize {
        self.handlers.len()
    }

    pub fn disconnect_hooks(&self) -> &[DisconnectHook] {
        &self.disconnect
    }
}
