//! `Services/WebApi`: the HTTP API of the game server (`WebApiService`, `WebApiServer`,
//! `WebApiSession`, `RouteMapper`, the controllers and their JSON models).
//!
//! The transport is axum (HTTP/1.1); every request goes through [`WebApiSession`]
//! (`OnReceivedRequest`), which keeps the C# routing (regex routes on the raw URL), status
//! codes and bodies.

mod auth;
pub mod controllers;
pub mod hooks;
pub mod http;
pub mod json;
pub mod route_mapper;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, Weak};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use tokio_util::sync::CancellationToken;

use self::http::{error_model, json_response, HttpRequest, HttpResponse};
use self::route_mapper::{ControllerCtx, Method, RouteMapper};
use crate::config::WebApiConfig;
use crate::server::GameServer;

/// `WebApiServer`: the routes plus the server the controllers reach.
pub struct WebApiServer {
    pub route_mapper: RouteMapper,
    server: Weak<GameServer>,
    /// Snapshot at listener creation; rotating the key requires a WebAPI restart.
    api_key: Option<String>,
}

impl WebApiServer {
    /// `new WebApiServer(address, port)`: discovers every controller route.
    pub fn new(server: Weak<GameServer>) -> Result<Self, route_mapper::RouteError> {
        let mut route_mapper = RouteMapper::new();
        route_mapper.discover_routes()?;
        Ok(Self::with_routes(route_mapper, server))
    }

    /// `new WebApiServer(address, port, routeMapper)`.
    pub fn with_routes(route_mapper: RouteMapper, server: Weak<GameServer>) -> Self {
        let api_key = server.upgrade().and_then(|s| {
            s.config
                .web_api_network
                .as_ref()
                .filter(|c| c.enabled)
                .and_then(|c| c.api_key.clone())
        });
        Self {
            route_mapper,
            server,
            api_key,
        }
    }
}

/// `WebApiSession.OnReceivedRequest(request)`: routes the request and returns the response
/// (`InternalSendResponseAsync` sends it).
pub async fn on_received_request(server: &WebApiServer, request: HttpRequest) -> HttpResponse {
    let result = async {
        let found = server
            .route_mapper
            .get_route(&request.url, &request.method)?;
        let Some((route, matches)) = found else {
            let mut response = HttpResponse::new(404);
            response.set_body("Not found");
            return Ok(response);
        };
        // Check the selected route, not a URL prefix: legacy regexes also match inside
        // query strings and prefixed paths. Only the read-only status handler is public.
        let public_status = route.method == Method::Get && route.path == "/status";
        if !(public_status || auth::authorized(server.api_key.as_deref(), &request.headers)) {
            let mut response = json_response(401, &error_model("Unauthorized"));
            response.set_header("WWW-Authenticate", "Bearer realm=\"AAEmu Web API\"");
            return Ok(response);
        }
        let target = route.target.clone();
        let ctx = ControllerCtx {
            server: server.server.upgrade(),
            request: request.clone(),
            matches,
        };
        target(ctx).await
    }
    .await;
    match result {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("{e:?}");
            internal_server_error(&e)
        }
    }
}

/// The 500 page of `OnReceivedRequest`'s `catch` (the stack trace is the error chain).
pub fn internal_server_error(e: &anyhow::Error) -> HttpResponse {
    let mut response = HttpResponse::new(500);
    response.set_content_type("text/html");
    let trace: Vec<String> = e.chain().skip(1).map(|c| format!("   at {c}")).collect();
    let html = format!(
        "<h1>Internal server error</h1>\n                   <h3>Error:{}</h3>\n                   <h2>Stack trace:</h3>\n                   <pre>{}</pre>",
        e,
        trace.join("\n")
    );
    response.set_body(html);
    response
}

/// Converts a response for axum (status, headers, body; `Content-Length` is added by hyper).
fn to_axum(r: HttpResponse) -> Response {
    let mut builder = Response::builder().status(r.status);
    for (k, v) in &r.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .body(Body::from(r.body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn dispatch(
    State(server): State<Arc<WebApiServer>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let url = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_string(), |p| p.as_str().to_string());
    let bytes: Bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let request = HttpRequest {
        method: parts.method.as_str().to_string(),
        url,
        protocol: format!("{:?}", parts.version),
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    };
    to_axum(on_received_request(&server, request).await)
}

/// `WebApiService` (`IHostedService`): starts / stops the HTTP listener.
#[derive(Default)]
pub struct WebApiService {
    running: Mutex<Option<(CancellationToken, tokio::task::JoinHandle<()>)>>,
}

impl std::fmt::Debug for WebApiService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebApiService")
            .field("started", &self.is_started())
            .finish()
    }
}

/// `config.Host.Equals("*") ? IPAddress.Any : IPAddress.Parse(config.Host)`.
pub fn bind_address(config: &WebApiConfig) -> anyhow::Result<SocketAddr> {
    auth::validate_key(config.api_key.as_deref())?;
    let ip: IpAddr = if config.host == "*" {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        config
            .host
            .parse()
            .map_err(|_| anyhow::anyhow!("An invalid IP address was specified."))?
    };
    anyhow::ensure!(
        ip.is_loopback() || config.api_key.as_ref().is_some_and(|key| !key.is_empty()),
        "WebApiNetwork.ApiKey is required for a non-loopback Host; use 127.0.0.1 for local status tooling or set Enabled to false"
    );
    Ok(SocketAddr::new(ip, config.port))
}

impl WebApiService {
    /// `StartAsync`: nothing without a `WebApiNetwork` section.
    pub async fn start(&self, server: &Arc<GameServer>) -> anyhow::Result<()> {
        let Some(config) = server.config.web_api_network.clone() else {
            tracing::warn!("WebApi server configuration not found. WebApi will not start");
            return Ok(());
        };
        if !config.enabled {
            tracing::info!("WebApi server disabled by configuration");
            return Ok(());
        }
        let addr = bind_address(&config)?;
        let mut api = WebApiServer::new(Arc::downgrade(server))?;
        // Use the same snapshot that passed the bind check even during a config reload.
        api.api_key = config.api_key.clone();
        let api = Arc::new(api);
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                // NetCoreServer reports socket errors through OnError and keeps running.
                tracing::warn!("WebApi server caught an error with code {e}");
                return Ok(());
            }
        };
        let token = CancellationToken::new();
        let stop = token.clone();
        let app = axum::Router::new().fallback(dispatch).with_state(api);
        let task = tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                stop.cancelled().await;
            });
            if let Err(e) = serve.await {
                tracing::warn!("WebApi server caught an error with code {e}");
            }
        });
        *self.running.lock().expect("webapi poisoned") = Some((token, task));
        tracing::info!("WebApi server started on {}:{}", config.host, config.port);
        Ok(())
    }

    /// `StopAsync`.
    pub async fn stop(&self) {
        let running = self.running.lock().expect("webapi poisoned").take();
        if let Some((token, task)) = running {
            token.cancel();
            let _ = task.await;
        }
        tracing::info!("WebApi server stopped");
    }

    /// `_server?.IsStarted`.
    pub fn is_started(&self) -> bool {
        self.running.lock().map(|r| r.is_some()).unwrap_or(false)
    }
}

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod session_tests;
