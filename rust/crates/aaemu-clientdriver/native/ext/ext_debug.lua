-- Built-in diagnostics feature "ext.debug": a live round-trip probe for the extension channel.
--   server -> client  ext.debug.ping { n = <number> }   client answers  ext.debug.pong { n = <number> }
-- AAEmuExt.Debug.Ping(n) sends ext.debug.ping to the server (a server that does not implement it
-- answers ext.error unknown_channel, which is logged).
AAEmuExt.Module{
  name = 'ext_debug',
  features = { 'ext.debug' },
  init = function(X)
    X.On('ext.debug.ping', function(msg)
      X.Emit('ext.debug.pong', { n = msg.n })
    end)
    X.On('ext.debug.pong', function(msg)
      X.Log('ext.debug.pong n=' .. tostring(msg.n))
    end)
    X.Debug = {
      Ping = function(n) return X.Emit('ext.debug.ping', { n = n or 0 }) end,
    }
  end,
}
