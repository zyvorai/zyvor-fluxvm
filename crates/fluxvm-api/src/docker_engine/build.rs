// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `POST /build` on the Docker socket: the build context is spooled to a scratch folder, unpacked, and handed to the
//! [`DockerBuilder`](crate::docker_build::DockerBuilder); progress goes back in Docker's JSON stream. The logic that needs no
//! socket is in `crate::docker_build`.

use super::{DockerError, DockerState};
use crate::docker_build::{
    self as db, BuildJob, BuildRecord, MAX_CONTEXT_BYTES, Scratch, aux_line, error_line,
    extract_context, short_id, stream_line,
};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

fn refused(r: db::Refusal) -> DockerError {
    DockerError::new(
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_REQUEST),
        r.message,
    )
}

/// The refusal for everything BuildKit: `/grpc`, `/session`, and buildx's attempt to start a `moby/buildkit` container.
pub(super) fn buildkit_refusal() -> DockerError {
    DockerError::new(
        StatusCode::NOT_IMPLEMENTED,
        "docker is trying to build with BuildKit (`docker buildx build`, or a `docker build` that uses buildx), which FluxVM's Docker socket does not serve. \
         Run `DOCKER_BUILDKIT=0 docker build -t NAME:TAG .` (put `export DOCKER_BUILDKIT=0` in your shell profile to make that the default), \
         or build with FluxVM directly: `fluxctl mac build -t NAME:TAG .` (it also takes `--secret` and `--ssh default`).",
    )
}

/// buildx's default builder starts BuildKit as a container from this image; it can never work here, so say so before pulling it.
pub(super) fn is_buildkit_image(image: &str) -> bool {
    let i = image.strip_prefix("docker.io/").unwrap_or(image);
    i.starts_with("moby/buildkit")
}

/// BuildKit's `/grpc` and `/session`: said plainly, once.
pub(super) async fn buildkit() -> DockerError {
    buildkit_refusal()
}

pub(super) async fn build(
    State(st): State<DockerState>,
    Query(pairs): Query<Vec<(String, String)>>,
    body: Body,
) -> Result<Response, DockerError> {
    let request = db::parse_query(&pairs).map_err(refused)?;
    let engine = st.build.clone();
    if let Some(why) = engine.builder.unavailable() {
        return Err(DockerError::new(StatusCode::NOT_IMPLEMENTED, why));
    }
    let scratch = Scratch::create().map_err(|e| {
        DockerError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not make a scratch folder for the build ({e}). Free some space in the temp folder and run the build again."),
        )
    })?;
    // The whole context first: the Docker CLI sends all of it before it reads the answer.
    let tar_path = scratch.0.join("context.tar");
    let mut sent: u64 = 0;
    {
        let mut f = tokio::fs::File::create(&tar_path).await.map_err(|e| {
            DockerError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not store the build context ({e})."),
            )
        })?;
        let mut s = body.into_data_stream();
        while let Some(chunk) = s.next().await {
            let chunk = chunk.map_err(|e| {
                DockerError::bad(format!(
                    "The build context upload was cut off ({e}). Run the build again."
                ))
            })?;
            sent += chunk.len() as u64;
            if sent > MAX_CONTEXT_BYTES {
                return Err(DockerError::bad(
                    "The build context is larger than 8 GiB. Add a `.dockerignore` for the big folders (build output, .git), then run the build again.",
                ));
            }
            f.write_all(&chunk).await.map_err(|e| {
                DockerError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Could not store the build context ({e})."),
                )
            })?;
        }
        f.flush().await.ok();
    }
    if sent == 0 {
        return Err(DockerError::bad(
            "The build had no context. Run `docker build -t NAME:TAG .` from the folder that holds the Macfile.",
        ));
    }
    let ctx_dir = scratch.0.join("context");
    let (dockerfile, tar2, ctx2) = (
        request.dockerfile.clone(),
        tar_path.clone(),
        ctx_dir.clone(),
    );
    let build_file = tokio::task::spawn_blocking(move || -> Result<String, String> {
        std::fs::create_dir_all(&ctx2)
            .map_err(|e| format!("Could not make the build folder ({e})."))?;
        extract_context(&tar2, &ctx2, MAX_CONTEXT_BYTES)?;
        let _ = std::fs::remove_file(&tar2);
        let file = db::find_build_file(&ctx2, &dockerfile)?;
        db::apply_dockerignore(&ctx2, &file)
            .map_err(|e| format!("Could not apply .dockerignore ({e})."))?;
        Ok(file)
    })
    .await
    .map_err(|e| {
        DockerError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("The build was interrupted ({e})."),
        )
    })?
    .map_err(DockerError::bad)?;

    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
    let started = db::now_unix();
    engine.history.start(BuildRecord {
        id: id.clone(),
        started_unix: started,
        finished_unix: None,
        duration_ms: None,
        state: "running".into(),
        tags: request.tags.clone(),
        dockerfile: build_file.clone(),
        build_arg_names: request.build_args.keys().cloned().collect(),
        target: request.target.clone(),
        platform: request.platform.clone(),
        no_cache: request.no_cache,
        context_bytes: sent,
        image_id: None,
        error: None,
        builder: engine.builder.name().into(),
        fluxctl: db::fluxctl_equivalent(&request),
    });

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let job = BuildJob {
        request,
        context_dir: ctx_dir,
        build_file,
    };
    tokio::spawn(run(engine, job, id, tx, scratch));
    let stream = async_stream::stream! {
        while let Some(b) = rx.recv().await {
            yield Ok::<Bytes, std::convert::Infallible>(b);
        }
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap())
}

async fn run(
    engine: Arc<db::BuildEngine>,
    job: BuildJob,
    id: String,
    tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    scratch: Scratch,
) {
    let quiet = job.request.quiet;
    let send = |s: String| {
        let _ = tx.send(Bytes::from(s));
    };
    if !quiet {
        send(stream_line(&format!(
            "Step 1/1 : FluxVM build of {} (same as `{}`)",
            job.request
                .tags
                .first()
                .map(String::as_str)
                .unwrap_or("an untagged image"),
            db::fluxctl_equivalent(&job.request)
        )));
    }
    let progress = |line: String| {
        if !quiet {
            let _ = tx.send(Bytes::from(stream_line(&line)));
        }
    };
    let t0 = std::time::Instant::now();
    let outcome = {
        let fut = engine.builder.build(&job, &progress);
        tokio::select! {
            r = fut => Some(r),
            // The docker CLI went away (Ctrl-C): dropping the future stops the build.
            _ = tx.closed() => None,
        }
    };
    let ms = t0.elapsed().as_millis() as u64;
    let fin = |state: &str, image: Option<String>, error: Option<String>| {
        engine.history.finish(&id, |r| {
            r.state = state.into();
            r.finished_unix = Some(db::now_unix());
            r.duration_ms = Some(ms);
            r.image_id = image;
            r.error = error;
        });
    };
    match outcome {
        None => fin(
            "cancelled",
            None,
            Some("The docker command was stopped before the build finished.".into()),
        ),
        Some(Err(msg)) => {
            fin("failed", None, Some(msg.clone()));
            send(error_line(&msg));
        }
        Some(Ok(img)) => {
            fin("succeeded", Some(img.id.clone()), None);
            send(aux_line(&img.id));
            if !quiet {
                send(stream_line(&format!(
                    "Successfully built {}",
                    short_id(&img.id)
                )));
                for t in &job.request.tags {
                    send(stream_line(&format!("Successfully tagged {t}")));
                }
            }
        }
    }
    drop(scratch);
}

#[cfg(test)]
mod tests {
    use super::super::router_with_build;
    use crate::docker_build::{BuildEngine, BuildFuture, BuiltImage, DockerBuilder, History};
    use axum::{Router, body::Body, http::Request};
    use fluxvm_core::config::Config;
    use fluxvm_scheduler::VmManager;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// A builder that builds nothing: it records what it was given and answers as told.
    #[derive(Default)]
    struct Fake {
        fail: Option<String>,
        unavailable: Option<String>,
        seen: Mutex<Vec<(crate::docker_build::BuildRequest, Vec<String>, String)>>,
    }

    fn files_in(dir: &std::path::Path, rel: &str, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let r = format!("{rel}{}", e.file_name().to_string_lossy());
            if e.file_type().unwrap().is_dir() {
                files_in(&e.path(), &format!("{r}/"), out);
            } else {
                out.push(r);
            }
        }
    }

    impl DockerBuilder for Fake {
        fn name(&self) -> &'static str {
            "fake builder"
        }
        fn unavailable(&self) -> Option<String> {
            self.unavailable.clone()
        }
        fn build<'a>(
            &'a self,
            job: &'a crate::docker_build::BuildJob,
            progress: &'a (dyn Fn(String) + Send + Sync),
        ) -> BuildFuture<'a> {
            Box::pin(async move {
                let mut files = Vec::new();
                files_in(&job.context_dir, "", &mut files);
                files.sort();
                self.seen.lock().unwrap().push((
                    job.request.clone(),
                    files,
                    job.build_file.clone(),
                ));
                progress("compiling the thing".into());
                match &self.fail {
                    Some(m) => Err(m.clone()),
                    None => Ok(BuiltImage {
                        id: format!("sha256:{}", "ab".repeat(32)),
                    }),
                }
            })
        }
    }

    async fn app(fake: Arc<Fake>) -> (Router, Arc<BuildEngine>) {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let cfg = Config {
            state_dir: dir.path().join("state"),
            run_dir: dir.path().join("run"),
            ..Config::default()
        };
        let m = VmManager::new(cfg).unwrap();
        let engine = Arc::new(BuildEngine::new(fake, Arc::new(History::new(None))));
        (router_with_build(m, engine.clone()), engine)
    }

    fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (p, d) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(d.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, p, *d).unwrap();
        }
        b.into_inner().unwrap()
    }

    async fn post(app: &Router, uri: &str, body: Vec<u8>) -> (u16, String) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn lines(body: &str) -> Vec<Value> {
        body.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn msg(body: &str) -> String {
        serde_json::from_str::<Value>(body).unwrap()["message"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }

    #[tokio::test]
    async fn a_build_streams_docker_json_returns_the_id_and_lists_the_image() {
        let fake = Arc::new(Fake::default());
        let (app, engine) = app(fake.clone()).await;
        let ctx = tar_of(&[
            ("Dockerfile", b"FROM base:15\n"),
            (".dockerignore", b"*.log\n"),
            ("src/a.txt", b"a"),
            ("x.log", b"noise"),
        ]);
        let uri = "/v1.43/build?t=app%3A1&t=app%3Alatest&buildargs=%7B%22VER%22%3A%223%22%7D&target=rel&platform=darwin%2Farm64&nocache=1&rm=1";
        let (s, body) = post(&app, uri, ctx).await;
        assert_eq!(s, 200, "{body}");
        let v = lines(&body);
        let streams: Vec<&str> = v.iter().filter_map(|l| l["stream"].as_str()).collect();
        assert!(
            streams[0].starts_with("Step 1/1 : FluxVM build of app:1"),
            "{streams:?}"
        );
        assert!(streams.contains(&"compiling the thing\n"));
        assert!(streams.contains(&"Successfully built abababababab\n"));
        assert!(
            streams.contains(&"Successfully tagged app:1\n")
                && streams.contains(&"Successfully tagged app:latest\n")
        );
        let id = format!("sha256:{}", "ab".repeat(32));
        assert!(v.iter().any(|l| l["aux"]["ID"] == id.as_str()));
        // The builder saw the unpacked, .dockerignore-filtered context and every flag.
        let seen = fake.seen.lock().unwrap();
        let (req, files, file) = &seen[0];
        assert_eq!(files, &[".dockerignore", "Dockerfile", "src/a.txt"]);
        assert_eq!(file, "Dockerfile");
        assert_eq!(req.tags, ["app:1", "app:latest"]);
        assert_eq!(req.build_args["VER"], "3");
        assert_eq!(req.target.as_deref(), Some("rel"));
        assert_eq!(req.platform.as_deref(), Some("darwin/arm64"));
        assert!(req.no_cache);
        drop(seen);
        // History: succeeded, with names of build args only.
        let h = engine.history.list();
        assert_eq!(h[0].state, "succeeded");
        assert_eq!(h[0].image_id.as_deref(), Some(id.as_str()));
        assert_eq!(h[0].build_arg_names, ["VER"]);
        assert!(!serde_json::to_string(&h).unwrap().contains("\"3\""));
        // `docker images` shows it ...
        let req = Request::builder()
            .uri("/v1.43/images/json")
            .body(Body::empty())
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let imgs: Value = serde_json::from_slice(&b).unwrap();
        let mine = imgs
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Id"] == id.as_str())
            .unwrap();
        assert_eq!(mine["RepoTags"], serde_json::json!(["app:1", "app:latest"]));
        // ... and `docker run` of it says plainly what to do instead.
        let req = Request::builder()
            .method("POST")
            .uri("/v1.43/containers/create")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"Image":"app:1"}"#))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status().as_u16(), 501);
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert!(msg(&String::from_utf8_lossy(&b)).contains("fluxctl mac run app:1"));
    }

    #[tokio::test]
    async fn a_failed_build_ends_the_stream_with_the_error_and_is_recorded() {
        let fake = Arc::new(Fake {
            fail: Some("step 2 failed: no space left".into()),
            ..Default::default()
        });
        let (app, engine) = app(fake).await;
        let (s, body) = post(&app, "/build?t=bad", tar_of(&[("Dockerfile", b"FROM x\n")])).await;
        assert_eq!(s, 200);
        let v = lines(&body);
        let last = v.last().unwrap();
        assert_eq!(
            last["errorDetail"]["message"],
            "step 2 failed: no space left"
        );
        assert_eq!(last["error"], "step 2 failed: no space left");
        assert!(v.iter().all(|l| l.get("aux").is_none()));
        let h = engine.history.list();
        assert_eq!(h[0].state, "failed");
        assert_eq!(h[0].error.as_deref(), Some("step 2 failed: no space left"));
        assert_eq!(engine.history.image_for("bad"), None);
    }

    #[tokio::test]
    async fn quiet_builds_send_only_the_id() {
        let (app, _) = app(Arc::new(Fake::default())).await;
        let (_, body) = post(
            &app,
            "/build?t=q&q=1",
            tar_of(&[("Dockerfile", b"FROM x\n")]),
        )
        .await;
        let v = lines(&body);
        assert_eq!(v.len(), 1);
        assert!(v[0]["aux"]["ID"].as_str().unwrap().starts_with("sha256:"));
    }

    #[tokio::test]
    async fn unsupported_flags_are_refused_before_the_builder_runs() {
        let fake = Arc::new(Fake::default());
        let (app, engine) = app(fake.clone()).await;
        let (s, body) = post(
            &app,
            "/build?t=x&memory=1073741824",
            tar_of(&[("Dockerfile", b"FROM x\n")]),
        )
        .await;
        assert_eq!(s, 501);
        let m = msg(&body);
        assert!(
            m.contains("--memory") && m.contains("fluxctl mac build"),
            "{m}"
        );
        let (s, body) = post(&app, "/build?t=x&version=2", vec![]).await;
        assert_eq!(s, 501);
        assert!(msg(&body).contains("DOCKER_BUILDKIT=0"));
        assert!(fake.seen.lock().unwrap().is_empty());
        assert!(engine.history.list().is_empty());
    }

    #[tokio::test]
    async fn a_host_that_cannot_build_says_so() {
        let fake = Arc::new(Fake {
            unavailable: Some("this host is not a Mac. Run `fluxctl docker up`.".into()),
            ..Default::default()
        });
        let (app, _) = app(fake).await;
        let (s, body) = post(&app, "/build?t=x", tar_of(&[("Dockerfile", b"FROM x\n")])).await;
        assert_eq!(s, 501);
        assert!(msg(&body).contains("fluxctl docker up"));
    }

    #[tokio::test]
    async fn a_missing_build_file_and_an_empty_upload_are_explained() {
        let (app, _) = app(Arc::new(Fake::default())).await;
        let (s, body) = post(&app, "/build?t=x", tar_of(&[("README", b"hi")])).await;
        assert_eq!(s, 400);
        assert!(msg(&body).contains("Macfile"), "{}", msg(&body));
        let (s, body) = post(&app, "/build?t=x", vec![]).await;
        assert_eq!(s, 400);
        assert!(msg(&body).contains("no context"));
        let (s, body) = post(
            &app,
            "/build?t=x",
            b"definitely not a tar archive".repeat(40),
        )
        .await;
        assert_eq!(s, 400, "{body}");
    }

    #[tokio::test]
    async fn a_macfile_stands_in_for_a_missing_dockerfile_and_f_is_honoured() {
        let fake = Arc::new(Fake::default());
        let (app, _) = app(fake.clone()).await;
        post(&app, "/build?t=x", tar_of(&[("Macfile", b"FROM x\n")])).await;
        post(
            &app,
            "/build?t=y&dockerfile=ci%2FMacfile.ci",
            tar_of(&[("ci/Macfile.ci", b"FROM x\n")]),
        )
        .await;
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen[0].2, "Macfile");
        assert_eq!(seen[1].2, "ci/Macfile.ci");
    }

    #[tokio::test]
    async fn buildkit_is_declined_and_ping_asks_for_the_classic_builder() {
        let (app, _) = app(Arc::new(Fake::default())).await;
        let (s, body) = post(&app, "/v1.43/grpc", vec![]).await;
        assert_eq!(s, 501);
        assert!(
            msg(&body).contains("DOCKER_BUILDKIT=0") && msg(&body).contains("fluxctl mac build")
        );
        let (s, _) = post(&app, "/session", vec![]).await;
        assert_eq!(s, 501);
        // buildx's default builder pulls and runs moby/buildkit: refused before anything is pulled.
        let (s, body) = post(
            &app,
            "/v1.43/images/create?fromImage=moby%2Fbuildkit&tag=buildx-stable-1",
            vec![],
        )
        .await;
        assert_eq!(s, 501, "{body}");
        assert!(msg(&body).contains("DOCKER_BUILDKIT=0"));
        let req = Request::builder()
            .method("POST")
            .uri("/v1.43/containers/create")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"Image":"moby/buildkit:buildx-stable-1"}"#))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status().as_u16(), 501);
        let req = Request::builder()
            .uri("/_ping")
            .body(Body::empty())
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.headers()["Builder-Version"], "1");
    }

    /// The real docker CLI against a FluxVM socket started here (never the host's own daemon): `-H` names our socket, and the
    /// environment is cleared of `DOCKER_HOST`/`DOCKER_CONTEXT` with a temporary `DOCKER_CONFIG`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the docker CLI; run with --ignored"]
    async fn real_docker_cli_builds_and_lists_through_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("fluxvm-test.sock");
        let dcfg = dir.path().join("dockercfg");
        std::fs::create_dir_all(&dcfg).unwrap();
        let ctx = dir.path().join("proj");
        std::fs::create_dir_all(ctx.join("src")).unwrap();
        std::fs::write(ctx.join("Macfile"), "FROM base:15\nRUN true\n").unwrap();
        std::fs::write(ctx.join("src/a.txt"), "a").unwrap();
        std::fs::write(ctx.join("junk.log"), "x").unwrap();
        std::fs::write(ctx.join(".dockerignore"), "*.log\n").unwrap();
        let fake = Arc::new(Fake::default());
        let cfg = Config {
            state_dir: dir.path().join("state"),
            run_dir: dir.path().join("run"),
            ..Config::default()
        };
        let m = VmManager::new(cfg).unwrap();
        let engine = Arc::new(BuildEngine::new(fake.clone(), Arc::new(History::new(None))));
        tokio::spawn(crate::docker_engine::serve_unix(sock.clone(), m, engine));
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let host = format!("unix://{}", sock.display());
        for m in ["-I", "-i"] {
            let o = std::process::Command::new("curl")
                .args(["-s", m, "--unix-socket"])
                .arg(&sock)
                .arg("http://localhost/_ping")
                .output()
                .unwrap();
            eprintln!(
                "--- curl {m} /_ping ---\n{}",
                String::from_utf8_lossy(&o.stdout)
            );
        }
        let docker_env = |classic: bool, args: &[&str]| {
            let o = std::process::Command::new("docker")
                .arg("-H")
                .arg(&host)
                .args(args)
                .current_dir(&ctx)
                .env_remove("DOCKER_HOST")
                .env_remove("DOCKER_CONTEXT")
                .env("DOCKER_BUILDKIT", if classic { "0" } else { "" })
                .env("DOCKER_CONFIG", &dcfg)
                .output()
                .unwrap();
            (
                o.status.code(),
                String::from_utf8_lossy(&o.stdout).into_owned(),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            )
        };
        let docker = |args: &[&str]| docker_env(true, args);
        // The CLI's default is BuildKit (buildx): it must get a sentence that says what to do, not a crash.
        let (c, out, err) = tokio::task::block_in_place(|| {
            docker_env(false, &["build", "-f", "Macfile", "-t", "bk:1", "."])
        });
        eprintln!("--- default docker build (buildx) ---\n{out}\n{err}");
        assert_ne!(c, Some(0));
        assert!(
            err.contains("DOCKER_BUILDKIT=0") && err.contains("fluxctl mac build"),
            "{err}"
        );
        let (c, out, err) = tokio::task::block_in_place(|| {
            docker(&[
                "build",
                "-f",
                "Macfile",
                "-t",
                "app:1",
                "--build-arg",
                "VER=3",
                "--label",
                "a=b",
                ".",
            ])
        });
        eprintln!("--- docker build ---\n{out}\n{err}");
        assert_eq!(c, Some(0), "{out}\n{err}");
        assert!(
            out.contains("Successfully built abababababab")
                && out.contains("Successfully tagged app:1"),
            "{out}"
        );
        {
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen[0].1, [".dockerignore", "Macfile", "src/a.txt"]);
            assert_eq!(seen[0].0.build_args["VER"], "3");
            assert_eq!(seen[0].0.labels["a"], "b");
        }
        let (c, out, err) = tokio::task::block_in_place(|| docker(&["images"]));
        eprintln!("--- docker images ---\n{out}\n{err}");
        assert_eq!(c, Some(0));
        assert!(out.contains("app") && out.contains("abababababab"), "{out}");
        let (c, out, err) = tokio::task::block_in_place(|| {
            docker(&["build", "-f", "Macfile", "--memory", "1g", "-t", "x", "."])
        });
        eprintln!("--- refused ---\n{out}\n{err}");
        assert_ne!(c, Some(0));
        assert!(
            err.contains("--memory") && err.contains("fluxctl mac build"),
            "{err}"
        );
        let (c, out, err) = tokio::task::block_in_place(|| docker(&["run", "--rm", "app:1"]));
        eprintln!("--- run ---\n{out}\n{err}");
        assert_ne!(c, Some(0));
        assert!(err.contains("fluxctl mac run app:1"), "{err}");
    }
}
