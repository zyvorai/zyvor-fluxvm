// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Small OTLP/HTTP JSON exporter for public-edge spans.
//!
//! This intentionally emits the standard OTLP JSON envelope without pulling
//! the full OpenTelemetry SDK into the long-running edge process.

use crate::public::OtlpConfig;
use crate::{MeshError, Result};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct EdgeSpan {
    pub name: String,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub ok: bool,
    pub attributes: BTreeMap<String, String>,
}

pub async fn export(cfg: &OtlpConfig, span: EdgeSpan) -> Result<()> {
    let (trace_id, span_id) = ids()?;
    let attrs = span
        .attributes
        .into_iter()
        .map(|(key, value)| json!({"key":key,"value":{"stringValue":value}}))
        .collect::<Vec<Value>>();
    let body = json!({
      "resourceSpans":[{
        "resource":{"attributes":[
          {"key":"service.name","value":{"stringValue":cfg.service_name}},
          {"key":"service.namespace","value":{"stringValue":"zyvor.fluxmesh"}}
        ]},
        "scopeSpans":[{
          "scope":{"name":"fluxvm-mesh","version":"0.4.1"},
          "spans":[{
            "traceId":trace_id,
            "spanId":span_id,
            "name":span.name,
            "kind":1,
            "startTimeUnixNano":span.start_unix_nano.to_string(),
            "endTimeUnixNano":span.end_unix_nano.to_string(),
            "attributes":attrs,
            "status":{"code":if span.ok {1}else{2}}
          }]
        }]
      }]
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| MeshError::InvalidState(format!("build OTLP client: {e}")))?;
    let mut request = client.post(&cfg.endpoint).json(&body);
    for (name, value) in &cfg.headers {
        request = request.header(name, value);
    }
    request
        .send()
        .await
        .map_err(|e| MeshError::InvalidState(format!("OTLP trace export: {e}")))?
        .error_for_status()
        .map_err(|e| MeshError::InvalidState(format!("OTLP trace export: {e}")))?;
    Ok(())
}

fn ids() -> Result<(String, String)> {
    let rng = SystemRandom::new();
    let mut trace = [0u8; 16];
    let mut span = [0u8; 8];
    rng.fill(&mut trace)
        .map_err(|_| MeshError::InvalidState("generate OTLP trace id".into()))?;
    rng.fill(&mut span)
        .map_err(|_| MeshError::InvalidState("generate OTLP span id".into()))?;
    Ok((hex(&trace), hex(&span)))
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(H[(b >> 4) as usize] as char);
        out.push(H[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_have_otlp_width() {
        let (t, s) = ids().unwrap();
        assert_eq!(t.len(), 32);
        assert_eq!(s.len(), 16);
    }
}
