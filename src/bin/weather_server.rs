//! MCP server (stdio) that exposes a single tool — `get_forecast` — backed
//! by a durable Resonate workflow running on a separate worker process.
//!
//! Flow:
//!   Claude Desktop  -- stdio --> this MCP server  -- Resonate RPC --> worker
//!
//! The MCP server itself is stateless; durability lives in the Resonate
//! server. If this process is killed mid-call, restarting it and re-issuing
//! the request reconnects to the same in-flight workflow (deduplication by
//! promise ID — here, the lat/lon pair).

use std::sync::Arc;

use anyhow::Result;
use resonate::prelude::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars,
    transport::stdio,
    tool, tool_router, ErrorData as McpError, ServiceExt,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ForecastRequest {
    /// Latitude of the location (NWS only covers the United States).
    latitude: f64,
    /// Longitude of the location.
    longitude: f64,
}

/// Bridge between the MCP server and Resonate.
///
/// Holds an `Arc<Resonate>` so each tool call can dispatch to the worker
/// group via RPC. Cloning the bridge clones the Arc — cheap, safe across
/// concurrent tool calls.
#[derive(Clone)]
struct WeatherBridge {
    resonate: Arc<Resonate>,
}

#[tool_router(server_handler)]
impl WeatherBridge {
    pub fn new(resonate: Arc<Resonate>) -> Self {
        Self { resonate }
    }

    #[tool(
        description = "Get the National Weather Service forecast for a US location. \
                       Backed by a durable Resonate workflow — the call automatically \
                       retries on transient failure and survives process restarts."
    )]
    async fn get_forecast(
        &self,
        Parameters(ForecastRequest {
            latitude,
            longitude,
        }): Parameters<ForecastRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Promise ID = the input. Identical requests deduplicate on the
        // Resonate server: a second call with the same lat/lon while the
        // first is still in flight returns the same result rather than
        // re-running the workflow.
        let promise_id = format!("forecast-{latitude}-{longitude}");

        let result: String = self
            .resonate
            .rpc(&promise_id, "get_forecast", (latitude, longitude))
            .target("poll://any@workers")
            .await
            .map_err(|e| {
                McpError::internal_error(format!("resonate rpc failed: {e}"), None)
            })?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // MCP servers communicate over stdio, so logs MUST go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let resonate = Resonate::new(ResonateConfig {
        url: Some("http://localhost:8001".into()),
        group: Some("mcp-gateway".into()),
        ..Default::default()
    });

    let bridge = WeatherBridge::new(Arc::new(resonate));

    tracing::info!("starting Resonate MCP weather server on stdio");
    let service = bridge.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("MCP serve error: {e:?}");
    })?;

    service.waiting().await?;
    Ok(())
}
