// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Studio's Templates gallery and Cheatsheet pages.
//!
//! * `GET  /v1/stack-templates` lists the one-command stacks built into `fluxctl` (the same data as `fluxctl templates ls --json`),
//! * `GET  /v1/stack-templates/{name}` returns one with its file,
//! * `POST /v1/stack-templates/{name}/run` (admin) starts it by running `fluxctl templates run NAME --yes --json`, found next to the
//!   daemon or on PATH, so the Studio and the terminal do exactly the same thing. A Compose template needs a Docker engine
//!   and a k3s one needs a FluxVM cluster; when they are missing the answer says so in words (422) and changes nothing.
//! * `GET  /v1/cheatsheet` returns the grouped commands of `fluxctl cheatsheet --json`.

use crate::kube::err;
use crate::{ApiResult, Role, require_admin};
use axum::{Extension, Json, extract::Path, http::StatusCode};
use fluxvm_core::stack_templates as st;
use fluxvm_scheduler::VmManager;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/stack-templates", get(list))
        .route("/v1/stack-templates/{name}", get(one))
        .route("/v1/stack-templates/{name}/run", post(run))
        .route("/v1/cheatsheet", get(cheatsheet))
}

async fn list() -> Json<Value> {
    Json(st::gallery_json())
}

async fn cheatsheet() -> Json<Value> {
    Json(fluxvm_core::cheatsheet::json())
}

pub(crate) fn detail(name: &str) -> Option<Value> {
    let t = st::find(name)?;
    Some(json!({
        "name": t.name, "kind": t.kind, "title": t.title, "summary": t.summary, "after": t.after,
        "file": t.body, "run": format!("fluxctl templates run {}", t.name),
    }))
}

async fn one(Path(name): Path<String>) -> ApiResult<Json<Value>> {
    detail(&name).map(Json).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("there is no template called {name}; the gallery lists them"),
        )
    })
}

fn fluxctl() -> Option<PathBuf> {
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("fluxctl")))
        .filter(|p| p.is_file())
    {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("fluxctl"))
            .find(|p| p.is_file())
    })
}

async fn run(Extension(role): Extension<Role>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    // The name is looked up in the built-in list before anything is spawned, so it never reaches a command line unchecked.
    let Some(t) = st::find(&name) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("there is no template called {name}; the gallery lists them"),
        ));
    };
    let Some(bin) = fluxctl() else {
        return Err(err(
            StatusCode::NOT_IMPLEMENTED,
            "fluxctl was not found next to the daemon or on its PATH; run the same command in a terminal (Copy as fluxctl)",
        ));
    };
    let tname = t.name;
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(bin)
            .args(["templates", "run", tname, "--yes", "--json"])
            .stdin(std::process::Stdio::null())
            .output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not start fluxctl: {e}"),
        )
    })?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        return Ok(Json(
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"output": text.trim()})),
        ));
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    let msg = if msg.is_empty() {
        "fluxctl failed without a message".to_owned()
    } else {
        msg
    };
    Err(err(StatusCode::UNPROCESSABLE_ENTITY, msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detail_carries_the_file_and_the_command() {
        let d = detail("redis").unwrap();
        assert_eq!(d["kind"], "compose");
        assert!(d["file"].as_str().unwrap().contains("redis"));
        assert_eq!(d["run"], "fluxctl templates run redis");
        assert!(detail("../etc/passwd").is_none());
    }

    #[tokio::test]
    async fn the_gallery_and_the_cheatsheet_answer() {
        let Json(g) = list().await;
        assert!(g["templates"].as_array().unwrap().len() >= 5);
        let Json(c) = cheatsheet().await;
        assert!(c["groups"].as_array().unwrap().len() >= 3);
    }
}
