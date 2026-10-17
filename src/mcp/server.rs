use super::{McpController, protocol::McpService};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Guard {
    controller: McpController,
    authority: String,
    permits: Arc<tokio::sync::Semaphore>,
}

async fn guard(State(state): State<Guard>, mut request: Request<Body>, next: Next) -> Response {
    let Ok(_permit) = state.permits.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    if request.headers().get_all("host").iter().count() != 1
        || request.headers().get("host").and_then(|v| v.to_str().ok())
            != Some(state.authority.as_str())
        || request.headers().contains_key("origin")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.headers().get_all("authorization").iter().count() != 1 {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(principal) = token.and_then(|token| state.controller.authenticate(token)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !state.controller.allow_http(&principal) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    request.extensions_mut().insert(principal);
    match tokio::time::timeout(Duration::from_secs(10), next.run(request)).await {
        Ok(mut response) => {
            response.headers_mut().insert(
                "cache-control",
                axum::http::HeaderValue::from_static("no-store"),
            );
            response.headers_mut().insert(
                "x-content-type-options",
                axum::http::HeaderValue::from_static("nosniff"),
            );
            response
        }
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

pub(super) async fn run(
    controller: McpController,
    port: u16,
    generation: String,
    cancellation: CancellationToken,
) {
    let listener = match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
    {
        Ok(listener) => listener,
        Err(_) => {
            controller.server_ended(
                &generation,
                Some("Cannot listen on this local port. Choose another port.".into()),
            );
            return;
        }
    };
    if !controller.server_started(&generation) {
        return;
    }
    let authority = format!("127.0.0.1:{port}");
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts([authority.clone()])
        .with_allowed_origins(Vec::<String>::new())
        .enforce_origin_validation()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(256 * 1024)
        .with_cancellation_token(cancellation.clone());
    let handler_controller = controller.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(McpService {
                controller: handler_controller.clone(),
            })
        },
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            Guard {
                controller: controller.clone(),
                authority,
                permits: Arc::new(tokio::sync::Semaphore::new(16)),
            },
            guard,
        ));
    let timer_controller = controller.clone();
    let timer_cancel = cancellation.clone();
    let timer = tokio::spawn(async move {
        // Sessions last minutes; checking every few seconds is precise enough and keeps an idle server asleep.
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {_ = timer_cancel.cancelled()=>break,_=interval.tick()=>timer_controller.tick()}
        }
    });
    let shutdown = cancellation.clone();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.cancelled().await;
    });
    let result = serve.await;
    cancellation.cancel();
    let _ = timer.await;
    controller.server_ended(
        &generation,
        result
            .err()
            .map(|_| "The local MCP server stopped unexpectedly.".into()),
    );
}
