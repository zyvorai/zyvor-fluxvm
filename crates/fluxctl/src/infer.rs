// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Host inference helpers for `apple.host_gpu`: status against the daemon, and a stand-in OpenAI server.

use crate::remote::Remote;
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Value, json};
use std::net::SocketAddr;

#[derive(Subcommand)]
pub enum InferCommand {
    /// Probe `GET /v1/host/infer` on the daemon (reachable MLX / llama.cpp / stand-in?).
    Status,
    /// Run a tiny OpenAI-compatible stub on loopback (proves the vsock path without a real model).
    Serve {
        /// Bind address (default `127.0.0.1:8080`).
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Required; the stub only answers, it does not load weights.
        #[arg(long)]
        stand_in: bool,
    },
}

pub async fn run(r: &Remote, cmd: InferCommand) -> Result<()> {
    match cmd {
        InferCommand::Status => {
            let v = r.call(Method::GET, "/v1/host/infer", None).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            if v["models_ok"].as_bool() != Some(true) {
                anyhow::bail!(
                    "host inference is not ready: start mlx_lm.server / llama-server on port {}, or `fluxctl infer serve --stand-in`",
                    v["port"].as_u64().unwrap_or(8080)
                );
            }
            Ok(())
        }
        InferCommand::Serve { listen, stand_in } => {
            if !stand_in {
                anyhow::bail!(
                    "pass --stand-in (a real model server is mlx_lm.server or llama-server)"
                );
            }
            let addr: SocketAddr = listen
                .parse()
                .with_context(|| format!("listen address {listen:?}"))?;
            eprintln!("stand-in OpenAI API on http://{addr}/v1 (Ctrl-C to stop)");
            let app = Router::new()
                .route("/v1/models", get(models))
                .route("/v1/chat/completions", post(chat));
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
            Ok(())
        }
    }
}

async fn models() -> impl IntoResponse {
    Json(json!({
        "object": "list",
        "data": [{ "id": "stand-in-model", "object": "model" }],
    }))
}

async fn chat(Json(body): Json<Value>) -> impl IntoResponse {
    let last = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|a| a.last())
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    (
        StatusCode::OK,
        Json(json!({
            "object": "chat.completion",
            "model": "stand-in-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": format!("stand-in reply to: {last}"),
                },
            }],
        })),
    )
}
