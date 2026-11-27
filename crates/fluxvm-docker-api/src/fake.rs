// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Test-only in-memory [`DockerAdapter`]: lets the router's handler tests run without a daemon. Never shipped; the
//! daemon uses `fluxvm_api::docker_adapter::FluxvmAdapter`, which is tested against a real `VmManager`.

use crate::{
    AdapterError, AdapterResult, Container, ContainerDetails, CreateRequest, DockerAdapter,
    LogChunk, LogStream, Logs, LogsRequest, Transition,
};
use bytes::Bytes;
use futures::{FutureExt as _, future::BoxFuture, stream};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Default)]
struct Inner {
    /// Oldest first.
    containers: Vec<FakeContainer>,
    next: u64,
    last_create: Option<(Option<String>, CreateRequest)>,
    last_stop_timeout: Option<u64>,
    last_logs: Option<LogsRequest>,
}

struct FakeContainer {
    id: String,
    name: String,
    image: String,
    cmd: Option<Vec<String>>,
    tty: bool,
    state: &'static str,
    created: i64,
    logs: Vec<LogChunk>,
}

impl FakeContainer {
    fn summary(&self) -> Container {
        Container {
            id: self.id.clone(),
            names: vec![format!("/{}", self.name)],
            image: self.image.clone(),
            image_id: String::new(),
            command: self.cmd.as_deref().unwrap_or_default().join(" "),
            created: self.created,
            labels: BTreeMap::new(),
            state: self.state.into(),
            status: match self.state {
                "running" => "Up".into(),
                "created" => "Created".into(),
                _ => "Exited (0)".into(),
            },
        }
    }
}

/// Containers live in memory; `start`/`stop` flip their state and `logs` replays what [`FakeAdapter::push_log`]
/// recorded. IDs, names and ID prefixes resolve like Docker (exact name first).
#[derive(Default)]
pub struct FakeAdapter {
    inner: Mutex<Inner>,
}

impl FakeAdapter {
    /// Append a log line to container `id` (full ID).
    pub fn push_log(&self, id: &str, stream: LogStream, data: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some(c) = g.containers.iter_mut().find(|c| c.id == id) {
            c.logs.push(LogChunk {
                stream,
                data: Bytes::from(data.to_owned()),
            });
        }
    }

    /// The last `create` call's name and request.
    pub fn last_create(&self) -> Option<(Option<String>, CreateRequest)> {
        self.inner.lock().unwrap().last_create.clone()
    }

    /// The `t` of the last `stop` call.
    pub fn last_stop_timeout(&self) -> Option<u64> {
        self.inner.lock().unwrap().last_stop_timeout
    }

    /// The options of the last `logs` call.
    pub fn last_logs_request(&self) -> Option<LogsRequest> {
        self.inner.lock().unwrap().last_logs
    }

    fn with<T>(&self, id: &str, f: impl FnOnce(&mut FakeContainer) -> T) -> AdapterResult<T> {
        let mut g = self.inner.lock().unwrap();
        let name = id.trim_start_matches('/');
        let idx = match g
            .containers
            .iter()
            .position(|c| c.name == name || c.id == id)
        {
            Some(i) => i,
            None => {
                let matches: Vec<usize> = g
                    .containers
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| !id.is_empty() && c.id.starts_with(id))
                    .map(|(i, _)| i)
                    .collect();
                match matches[..] {
                    [i] => i,
                    [] => return Err(AdapterError::no_such_container(id)),
                    _ => {
                        return Err(AdapterError::bad_request(format!(
                            "Multiple containers match prefix {id}"
                        )));
                    }
                }
            }
        };
        Ok(f(&mut g.containers[idx]))
    }
}

impl DockerAdapter for FakeAdapter {
    fn list(&self, all: bool) -> BoxFuture<'_, AdapterResult<Vec<Container>>> {
        let g = self.inner.lock().unwrap();
        let out = g
            .containers
            .iter()
            .rev()
            .map(FakeContainer::summary)
            .filter(|c| all || c.is_up())
            .collect();
        futures::future::ready(Ok(out)).boxed()
    }

    fn inspect(&self, id: String) -> BoxFuture<'_, AdapterResult<ContainerDetails>> {
        let r = self.with(&id, |c| ContainerDetails {
            summary: c.summary(),
            tty: c.tty,
            cmd: c.cmd.clone(),
            exit_code: (c.state == "exited").then_some(0),
        });
        futures::future::ready(r).boxed()
    }

    fn create(
        &self,
        name: Option<String>,
        req: CreateRequest,
    ) -> BoxFuture<'_, AdapterResult<String>> {
        let mut g = self.inner.lock().unwrap();
        g.next += 1;
        let n = g.next;
        let name = name.unwrap_or_else(|| format!("fake_{n}"));
        if g.containers.iter().any(|c| c.name == name) {
            return futures::future::ready(Err(AdapterError::conflict(format!(
                "Conflict. The container name \"/{name}\" is already in use."
            ))))
            .boxed();
        }
        let id = format!("{n:064x}").chars().rev().collect::<String>();
        g.last_create = Some((Some(name.clone()), req.clone()));
        g.containers.push(FakeContainer {
            id: id.clone(),
            name,
            image: req.image,
            cmd: req.cmd,
            tty: req.tty,
            state: "created",
            created: 1_700_000_000 + i64::try_from(n).unwrap_or(0),
            logs: Vec::new(),
        });
        futures::future::ready(Ok(id)).boxed()
    }

    fn start(&self, id: String) -> BoxFuture<'_, AdapterResult<Transition>> {
        let r = self.with(&id, |c| {
            if c.state == "running" {
                Transition::Unchanged
            } else {
                c.state = "running";
                Transition::Changed
            }
        });
        futures::future::ready(r).boxed()
    }

    fn stop(&self, id: String, timeout: Option<u64>) -> BoxFuture<'_, AdapterResult<Transition>> {
        self.inner.lock().unwrap().last_stop_timeout = timeout;
        let r = self.with(&id, |c| {
            if c.state == "running" {
                c.state = "exited";
                Transition::Changed
            } else {
                Transition::Unchanged
            }
        });
        futures::future::ready(r).boxed()
    }

    fn logs(&self, id: String, req: LogsRequest) -> BoxFuture<'_, AdapterResult<Logs>> {
        self.inner.lock().unwrap().last_logs = Some(req);
        let r = self.with(&id, |c| {
            let wanted: Vec<LogChunk> = c
                .logs
                .iter()
                .filter(|l| match l.stream {
                    LogStream::Stdout => req.stdout,
                    LogStream::Stderr => req.stderr,
                })
                .cloned()
                .collect();
            let skip = req.tail.map_or(0, |n| wanted.len().saturating_sub(n));
            let chunks: Vec<_> = wanted.into_iter().skip(skip).map(Ok).collect();
            Logs {
                tty: c.tty,
                chunks: Box::pin(stream::iter(chunks)),
            }
        });
        futures::future::ready(r).boxed()
    }
}
