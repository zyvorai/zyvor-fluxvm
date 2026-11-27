// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Route tests: what each list returns for a filter, and that nothing unfiltered comes back for one it cannot honour.

use super::*;
use axum::http::Request;
use chrono::Duration as Age;
use fluxvm_core::config::Config;
use fluxvm_scheduler::oci_images::rootfs_dir;
use tower::ServiceExt as _;

const ALPINE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REDIS: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const LOOSE: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

struct Engine {
    _dir: tempfile::TempDir,
    m: Arc<VmManager>,
    app: Router,
}

/// An entry in the image cache, with an empty file standing in for its root filesystem.
fn cache_image(cfg: &Config, digest: &str, references: &[&str], built_at: u64) {
    let dir = rootfs_dir(cfg);
    std::fs::create_dir_all(&dir).unwrap();
    let hex = &digest["sha256:".len()..];
    std::fs::write(dir.join(format!("{hex}.ext4")), b"").unwrap();
    let entry = json!({
        "manifest_digest": digest, "references": references, "config_digest": digest,
        "config": {}, "layers": [], "size_bytes": 1, "built_at": built_at, "last_used": built_at,
    });
    std::fs::write(
        dir.join(format!("{hex}.json")),
        serde_json::to_vec(&entry).unwrap(),
    )
    .unwrap();
}

/// Containers, oldest first: `web` (running), `web-2` (exited 0), `db` (exited 3), `job` (created).
async fn engine() -> Engine {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        state_dir: dir.path().join("state"),
        run_dir: dir.path().join("run"),
        ..Config::default()
    };
    cache_image(
        &cfg,
        ALPINE,
        &["registry-1.docker.io/library/alpine:3.22"],
        100,
    );
    cache_image(
        &cfg,
        REDIS,
        &[
            "registry-1.docker.io/library/redis:7",
            "registry-1.docker.io/library/redis:latest",
        ],
        200,
    );
    cache_image(&cfg, LOOSE, &[], 300);
    let m = VmManager::new(cfg).unwrap();
    let rows = [
        ("web", VmStatus::Running, "alpine:3.22", ALPINE, None),
        ("web-2", VmStatus::Stopped, "alpine:3.22", ALPINE, Some(0)),
        ("db", VmStatus::Stopped, "redis:7", REDIS, Some(3)),
        (
            "job",
            VmStatus::Stopped,
            "ghcr.io/org/job:1",
            "sha256:ddd",
            None,
        ),
    ];
    for (age, (name, status, image, digest, exit)) in rows.into_iter().enumerate() {
        let mut vm = crate::docker_engine::vm_fixture(name, status, true);
        vm.id =
            Uuid::parse_str(&format!("{}0000000-0000-4000-8000-000000000000", age + 1)).unwrap();
        vm.created_at = Utc::now() - Age::seconds(100 - age as i64);
        vm.workspace = dir.path().join(name);
        std::fs::create_dir_all(&vm.workspace).unwrap();
        vm.log_path = vm.workspace.join("console.log");
        vm.labels.insert(OCI_IMAGE_LABEL.into(), image.into());
        vm.labels.insert(OCI_LABEL.into(), digest.into());
        if name == "job" {
            vm.labels.insert(
                fluxvm_scheduler::oci_sandbox::CREATED_LABEL.into(),
                "true".into(),
            );
        }
        // `db` exits with 3: read from a container VM's console.
        if exit == Some(3) {
            vm.request.apple = Some(serde_json::from_value(json!({ "init_config": {} })).unwrap());
            std::fs::write(&vm.log_path, "FLUXVM-EXIT 3\n").unwrap();
        }
        let labels = match name {
            "web" => vec![("tier", "front"), ("ci", "1")],
            "web-2" => vec![("tier", "front")],
            "db" => vec![("tier", "back"), ("ci", "1")],
            _ => vec![],
        };
        let labels = labels
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        store_labels(&vm, &labels).await.unwrap();
        m.store.insert(vm).await.unwrap();
    }
    Engine {
        _dir: dir,
        app: router(m.clone()),
        m,
    }
}

async fn call(e: &Engine, method: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = e.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| json!(String::from_utf8_lossy(&body).into_owned())),
    )
}

/// Percent-encoded, as the CLI sends `filters`.
fn encoded(raw: &str) -> String {
    raw.bytes().map(|b| format!("%{b:02X}")).collect()
}

/// `{"key": {"value": true}}` from (key, value) pairs.
fn filter_json(pairs: &[(&str, &str)]) -> String {
    let mut map: BTreeMap<&str, BTreeMap<&str, bool>> = BTreeMap::new();
    for (k, v) in pairs {
        map.entry(k).or_default().insert(v, true);
    }
    serde_json::to_string(&map).unwrap()
}

async fn list(e: &Engine, path: &str, query: &str, pairs: &[(&str, &str)]) -> Value {
    let uri = format!(
        "/v1.44/{path}?{query}&filters={}",
        encoded(&filter_json(pairs))
    );
    let (status, body) = call(e, "GET", &uri).await;
    assert_eq!(status, StatusCode::OK, "{pairs:?}: {body}");
    body
}

/// The names `docker ps` would show, newest first.
async fn ps(e: &Engine, query: &str, pairs: &[(&str, &str)]) -> Vec<String> {
    list(e, "containers/json", query, pairs)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["Names"][0].as_str().unwrap()[1..].to_string())
        .collect()
}

#[tokio::test]
async fn name_and_id_filters_narrow_like_docker() {
    let e = engine().await;
    // The reported hazard: this fed `docker rm -f` with every container.
    assert!(ps(&e, "all=1", &[("name", "x")]).await.is_empty());
    assert_eq!(ps(&e, "all=1", &[("name", "web")]).await, ["web-2", "web"]);
    assert_eq!(ps(&e, "all=1", &[("name", "^web$")]).await, ["web"]);
    assert_eq!(ps(&e, "all=1", &[("name", "^/db$")]).await, ["db"]);
    assert_eq!(
        ps(&e, "all=1", &[("name", "db"), ("name", "job")]).await,
        ["job", "db"]
    );
    // Without -a only running containers.
    assert_eq!(ps(&e, "", &[("name", "web")]).await, ["web"]);

    assert_eq!(ps(&e, "all=1", &[("id", "3000")]).await, ["db"]);
    assert_eq!(
        ps(&e, "all=1", &[("id", "3000"), ("id", "1")]).await,
        ["db", "web"]
    );
    assert!(ps(&e, "all=1", &[("id", "000")]).await.is_empty());
    assert!(ps(&e, "all=1", &[("id", "")]).await.is_empty());
    // Keys must all match.
    assert!(
        ps(&e, "all=1", &[("id", "3000"), ("name", "web")])
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn an_ambiguous_id_prefix_selects_nothing() {
    let e = engine().await;
    for n in [5, 6] {
        let mut vm = crate::docker_engine::vm_fixture(&format!("twin{n}"), VmStatus::Running, true);
        vm.id = Uuid::parse_str(&format!("9{n}000000-0000-4000-8000-000000000000")).unwrap();
        e.m.store.insert(vm).await.unwrap();
    }
    assert!(ps(&e, "all=1", &[("id", "9")]).await.is_empty());
    assert_eq!(ps(&e, "all=1", &[("id", "95")]).await, ["twin5"]);
}

#[tokio::test]
async fn status_and_exit_code_filters() {
    let e = engine().await;
    // A status filter looks at stopped containers without -a.
    assert_eq!(ps(&e, "", &[("status", "exited")]).await, ["db", "web-2"]);
    assert_eq!(ps(&e, "", &[("status", "created")]).await, ["job"]);
    assert_eq!(ps(&e, "", &[("status", "running")]).await, ["web"]);
    assert_eq!(
        ps(&e, "", &[("status", "created"), ("status", "running")]).await,
        ["job", "web"]
    );
    assert!(ps(&e, "all=1", &[("status", "dead")]).await.is_empty());
    assert_eq!(ps(&e, "all=1", &[("exited", "3")]).await, ["db"]);
    assert_eq!(ps(&e, "all=1", &[("exited", "0")]).await, ["web-2"]);
    // As Docker: `exited` alone does not imply -a.
    assert!(ps(&e, "", &[("exited", "3")]).await.is_empty());
}

#[tokio::test]
async fn label_filters_see_the_labels_given_at_create() {
    let e = engine().await;
    assert_eq!(ps(&e, "all=1", &[("label", "ci")]).await, ["db", "web"]);
    assert_eq!(
        ps(&e, "all=1", &[("label", "tier=front")]).await,
        ["web-2", "web"]
    );
    // Every label value must match.
    assert_eq!(
        ps(&e, "all=1", &[("label", "ci=1"), ("label", "tier=front")]).await,
        ["web"]
    );
    assert!(ps(&e, "all=1", &[("label", "ci=2")]).await.is_empty());
    assert!(ps(&e, "all=1", &[("label", "nope")]).await.is_empty());
    let (_, inspect) = call(&e, "GET", "/v1.44/containers/web/json").await;
    assert_eq!(inspect["Config"]["Labels"]["tier"], "front");
}

#[tokio::test]
async fn ancestor_before_since_and_limit() {
    let e = engine().await;
    for ancestor in [
        "alpine:3.22",
        "docker.io/library/alpine:3.22",
        ALPINE,
        &"a".repeat(12),
    ] {
        assert_eq!(
            ps(&e, "all=1", &[("ancestor", ancestor)]).await,
            ["web-2", "web"],
            "{ancestor}"
        );
    }
    // `alpine` is `alpine:latest`, which nothing was made from.
    assert!(ps(&e, "all=1", &[("ancestor", "alpine")]).await.is_empty());
    assert!(ps(&e, "all=1", &[("ancestor", "nginx:1")]).await.is_empty());
    // Not in the cache (a native container's image): matched by reference.
    assert_eq!(
        ps(&e, "all=1", &[("ancestor", "ghcr.io/org/job:1")]).await,
        ["job"]
    );

    assert_eq!(ps(&e, "all=1", &[("before", "db")]).await, ["web-2", "web"]);
    assert_eq!(ps(&e, "all=1", &[("since", "web-2")]).await, ["job", "db"]);
    assert_eq!(
        ps(&e, "all=1", &[("since", "web"), ("before", "job")]).await,
        ["db", "web-2"]
    );
    // The limit counts matching containers.
    assert_eq!(ps(&e, "limit=1", &[("name", "web")]).await, ["web-2"]);
    // The old list format of `filters`.
    let uri = format!(
        "/containers/json?all=1&filters={}",
        encoded(r#"{"name":["db"]}"#)
    );
    let (_, v) = call(&e, "GET", &uri).await;
    assert_eq!(v.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_filter_that_cannot_be_honoured_never_returns_a_list() {
    let e = engine().await;
    let bad = StatusCode::BAD_REQUEST;
    let unsupported = StatusCode::NOT_IMPLEMENTED;
    for (raw, want) in [
        ("name=x", bad),
        ("[]", bad),
        ("null", bad),
        (r#"{"name":"x"}"#, bad),
        (r#"{"name":null}"#, bad),
        (r#"{"name":{}}"#, bad),
        (r#"{"name":[]}"#, bad),
        (r#"{"name":{"(":true}}"#, bad),
        (r#"{"nmae":{"x":true}}"#, bad),
        (r#"{"status":{"stopped":true}}"#, bad),
        (r#"{"exited":{"zero":true}}"#, bad),
        (r#"{"ancestor":{"Not A Ref":true}}"#, bad),
        (r#"{"before":{"ghost":true}}"#, StatusCode::NOT_FOUND),
        (r#"{"since":{"ghost":true}}"#, StatusCode::NOT_FOUND),
        (r#"{"network":{"bridge":true}}"#, unsupported),
        (r#"{"volume":{"data":true}}"#, unsupported),
        (r#"{"health":{"healthy":true}}"#, unsupported),
        (r#"{"publish":{"80":true}}"#, unsupported),
        (r#"{"expose":{"80":true}}"#, unsupported),
        (r#"{"isolation":{"default":true}}"#, unsupported),
        (r#"{"is-task":{"true":true}}"#, unsupported),
        // A good filter does not carry a bad one through.
        (r#"{"name":{"web":true},"network":{"x":true}}"#, unsupported),
    ] {
        let uri = format!("/v1.44/containers/json?all=1&filters={}", encoded(raw));
        let (status, body) = call(&e, "GET", &uri).await;
        assert_eq!(status, want, "{raw}: {body}");
        assert!(body["message"].is_string(), "{raw}: {body}");
    }
    for query in ["before=db", "since=db"] {
        let (status, _) = call(&e, "GET", &format!("/containers/json?{query}")).await;
        assert_eq!(status, bad, "{query}");
    }
    let (status, _) = call(&e, "GET", "/containers/json?filters=%7B%7D&filters=%7B%7D").await;
    assert_eq!(status, bad, "two filter parameters");
    for empty in ["", "%7B%7D"] {
        let (status, v) = call(
            &e,
            "GET",
            &format!("/containers/json?all=1&filters={empty}"),
        )
        .await;
        assert_eq!((status, v.as_array().unwrap().len()), (StatusCode::OK, 4));
    }
}

/// The ids `docker images` would show.
async fn images(e: &Engine, pairs: &[(&str, &str)]) -> Vec<String> {
    let mut ids: Vec<String> = list(e, "images/json", "", pairs)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn image_filters() {
    let e = engine().await;
    for pattern in ["alpine", "alpine:3.22", "alp*", "alpine:3.*", "a?pine"] {
        assert_eq!(
            images(&e, &[("reference", pattern)]).await,
            [ALPINE],
            "{pattern}"
        );
    }
    for pattern in ["alpine:latest", "pine", "*/alpine", "x"] {
        assert!(
            images(&e, &[("reference", pattern)]).await.is_empty(),
            "{pattern}"
        );
    }
    assert_eq!(
        images(&e, &[("reference", "alpine"), ("reference", "redis")]).await,
        [ALPINE, REDIS]
    );
    // Only the matching names are listed.
    let v = list(&e, "images/json", "", &[("reference", "redis:7")]).await;
    assert_eq!(
        v[0]["RepoTags"],
        json!(["registry-1.docker.io/library/redis:7"])
    );
    assert_eq!(images(&e, &[("dangling", "true")]).await, [LOOSE]);
    assert_eq!(images(&e, &[("dangling", "false")]).await, [ALPINE, REDIS]);
    assert_eq!(images(&e, &[("before", "redis:7")]).await, [ALPINE]);
    assert_eq!(images(&e, &[("since", ALPINE)]).await, [REDIS, LOOSE]);

    for (raw, want) in [
        ("junk", StatusCode::BAD_REQUEST),
        (r#"{"dangling":{"maybe":true}}"#, StatusCode::BAD_REQUEST),
        (
            r#"{"dangling":{"true":true,"false":true}}"#,
            StatusCode::BAD_REQUEST,
        ),
        (r#"{"reference":{"[":true}}"#, StatusCode::BAD_REQUEST),
        (r#"{"reference":{}}"#, StatusCode::BAD_REQUEST),
        (r#"{"name":{"x":true}}"#, StatusCode::BAD_REQUEST),
        (r#"{"since":{"ghost":true}}"#, StatusCode::NOT_FOUND),
        (r#"{"label":{"a":true}}"#, StatusCode::NOT_IMPLEMENTED),
        (r#"{"label!":{"a":true}}"#, StatusCode::NOT_IMPLEMENTED),
        (r#"{"until":{"24h":true}}"#, StatusCode::NOT_IMPLEMENTED),
    ] {
        let uri = format!("/v1.44/images/json?filters={}", encoded(raw));
        let (status, body) = call(&e, "GET", &uri).await;
        assert_eq!(status, want, "{raw}: {body}");
    }
    let (status, _) = call(&e, "GET", "/images/json?filter=alpine").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn reference_patterns_follow_path_match() {
    let matches = |p: &str, s: &str| path_pattern(p).unwrap().is_match(s);
    assert!(matches("a*", "abc") && !matches("a*", "a/b"));
    assert!(matches("*/*", "org/app") && !matches("?", "/"));
    assert!(matches("v[0-9]", "v7") && !matches("v[^0-9]", "v7"));
    assert!(matches(r"a\*", "a*") && !matches(r"a\*", "ab"));
    // Characters special to regular expressions are literal.
    assert!(matches("a.b+(c)", "a.b+(c)") && !matches("a.b", "axb"));
    for bad in ["[", "[]", "[]a]", "a\\", "[a-", "[-a]", "[b-a]"] {
        assert!(path_pattern(bad).is_none(), "{bad}");
    }
}

async fn remaining_images(e: &Engine) -> usize {
    e.m.oci_list().unwrap().len()
}

#[tokio::test]
async fn rmi_without_force_removes_only_what_was_named() {
    let e = engine().await;
    let hex = |d: &str| d["sha256:".len()..][..12].to_string();
    // Two names: removing by either name or by id would drop both.
    for what in [hex(REDIS), REDIS.to_string()] {
        let (status, v) = call(&e, "DELETE", &format!("/v1.44/images/{what}?force=0")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{what}: {v}");
    }
    // In use by containers.
    let (status, v) = call(&e, "DELETE", &format!("/v1.44/images/{}", hex(ALPINE))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("is using this image")
    );
    let uri = format!("/v1.44/images/{}?platforms=%7B%7D&force=1", hex(LOOSE));
    assert_eq!(
        call(&e, "DELETE", &uri).await.0,
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(remaining_images(&e).await, 3);

    let uri = format!("/v1.44/images/{}", hex(LOOSE));
    assert_eq!(call(&e, "DELETE", &uri).await.0, StatusCode::OK);
    let uri = format!("/v1.44/images/{}?force=1&noprune=1", hex(REDIS));
    assert_eq!(call(&e, "DELETE", &uri).await.0, StatusCode::OK);
    assert_eq!(remaining_images(&e).await, 1);
}

#[tokio::test]
async fn deletions_that_cannot_be_done_as_asked_delete_nothing() {
    let e = engine().await;
    let (status, _) = call(&e, "DELETE", "/v1.44/containers/db?link=1").await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    for prune in [
        "containers/prune",
        "images/prune",
        "networks/prune",
        "volumes/prune",
        "build/prune",
        "system/prune",
    ] {
        // With or without a filter the CLI adds, such as `until` or `label`.
        let uri = format!(
            "/v1.44/{prune}?filters={}",
            encoded(r#"{"label":{"a":true}}"#)
        );
        let (status, v) = call(&e, "POST", &uri).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{prune}: {v}");
    }
    assert_eq!(oci_containers(&e.m).await.len(), 4);
    assert_eq!(remaining_images(&e).await, 3);
}

#[tokio::test]
async fn network_and_volume_filters() {
    let e = engine().await;
    for (pairs, count) in [
        (vec![("name", "fluxvm")], 1),
        (vec![("name", "zzz")], 0),
        // Name filters are regular expressions, matched anywhere.
        (vec![("name", "x")], 1),
        (vec![("id", "5f1a")], 1),
        (vec![("id", "ffff")], 0),
        (vec![("driver", "user-mode-nat"), ("scope", "local")], 1),
        (vec![("driver", "bridge")], 0),
        (vec![("type", "builtin")], 1),
        (vec![("type", "custom")], 0),
        (vec![("dangling", "false")], 1),
        (vec![("dangling", "true")], 0),
        (vec![("label", "fluxvm.note")], 1),
        (vec![("label", "mine")], 0),
    ] {
        let v = list(&e, "networks", "", &pairs).await;
        assert_eq!(v.as_array().unwrap().len(), count, "{pairs:?}");
    }
    for pairs in [
        vec![("name", "x")],
        vec![("dangling", "true"), ("driver", "local")],
        vec![("label", "a=b")],
    ] {
        let v = list(&e, "volumes", "", &pairs).await;
        assert_eq!(v["Volumes"], json!([]), "{pairs:?}");
    }
    for (path, raw) in [
        ("networks", r#"{"type":{"odd":true}}"#),
        ("networks", r#"{"since":{"x":true}}"#),
        ("networks", "junk"),
        ("volumes", r#"{"dangling":{"maybe":true}}"#),
        ("volumes", r#"{"since":{"x":true}}"#),
        ("volumes", "junk"),
        ("events", r#"{"label":{"x":true}}"#),
        ("events", r#"{"event":{}}"#),
        ("events", r#"{"nope":{"x":true}}"#),
    ] {
        let uri = format!("/v1.44/{path}?until=1&filters={}", encoded(raw));
        let (status, v) = call(&e, "GET", &uri).await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_IMPLEMENTED,
            "{path} {raw}: {status} {v}"
        );
    }
}
