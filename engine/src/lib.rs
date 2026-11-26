//! The Kamroll engine.
//!
//! The desktop app links this library and talks to it inside its own process: there is no web server, no port
//! and no address anyone else can open. The command line (`kamroll-engine`) uses the same code.
//!
//! The optional `web` feature adds `kamroll-engine ui`, a loopback server used for development and by the tests
//! that compare this engine with the Python reference. The desktop app is built without it.
mod adopt;
mod afc;
mod ai;
mod analysis;
mod app;
mod backup;
mod cli;
mod config;
mod copier;
mod device;
mod edits;
mod filing;
mod layout;
mod legacy;
mod manifest;
mod metadata;
mod schema;
mod system;
mod tagging;
mod thumbs;
mod trips;
mod verify;

pub use cli::{run as run_command, Args as CommandArgs};
pub use config::migrate_settings;

use axum::body::Body;
use axum::http::{Request, Response};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use tower::ServiceExt;

fn random_token() -> String {
    (0..3).map(|_| format!("{:016x}", RandomState::new().build_hasher().finish())).collect()
}

/// The engine inside another program: hand it a request, get the response. Nothing listens on the network.
pub struct Embedded {
    router: axum::Router,
}

impl Embedded {
    pub fn new() -> Embedded {
        migrate_settings();
        Embedded { router: app::router(random_token(), true) }
    }

    pub async fn handle(&self, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        let (parts, body) = request.into_parts();
        let response = match self.router.clone().oneshot(Request::from_parts(parts, Body::from(body))).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.map(|b| b.to_vec()).unwrap_or_default();
        Response::from_parts(parts, bytes)
    }
}

impl Default for Embedded {
    fn default() -> Self {
        Self::new()
    }
}

/// Development and tests only: serve the app on 127.0.0.1.
#[cfg(feature = "web")]
pub fn serve_web(port: u16, token: Option<String>) -> std::io::Result<()> {
    migrate_settings();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let token = token.unwrap_or_else(random_token);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        println!("Kamroll development server: http://127.0.0.1:{port}/  (Ctrl+C to stop)");
        axum::serve(listener, app::router(token, false)).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn get(engine: &Embedded, uri: &str, token: Option<&str>) -> Response<Vec<u8>> {
        let mut request = Request::builder().uri(uri);
        if let Some(t) = token {
            request = request.header("x-photovault-token", t);
        }
        block_on(engine.handle(request.body(Vec::new()).unwrap()))
    }

    #[test]
    fn answers_inside_the_process_and_still_needs_the_token() {
        let engine = Embedded { router: app::router("secret".into(), true) };
        let page = get(&engine, "http://kamroll.localhost/?desktop=win", None);
        assert_eq!(page.status(), 200);
        assert!(String::from_utf8_lossy(page.body()).contains("secret"));
        assert_eq!(get(&engine, "kamroll://localhost/static/app.js", None).status(), 200);
        assert_eq!(get(&engine, "http://kamroll.localhost/api/health", None).status(), 401);
        let health = get(&engine, "kamroll://localhost/api/health", Some("secret"));
        assert_eq!(health.status(), 200);
        assert!(String::from_utf8_lossy(health.body()).contains("\"ok\":true"));
        // the endless event stream is not used inside the app
        assert_eq!(get(&engine, "http://kamroll.localhost/api/backup/events", Some("secret")).status(), 204);
    }
}
