//! `fluxctl kube add postgres`: the services a developer reaches for, running in the cluster with their usual port on
//! 127.0.0.1 and a connection string printed, like a package manager for local infrastructure. The Service carries a
//! `fluxvm.dev/local-ports` annotation, which is how the supervisor knows to open 5432 on the Mac for node port 30432.

use anyhow::{Result, bail};

use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum App {
    /// PostgreSQL 17 on localhost:5432 (user postgres, password dev).
    Postgres,
    /// Redis 7 on localhost:6379, with persistence.
    Redis,
    /// MySQL 8.4 on localhost:3306 (user root, password dev, database app).
    Mysql,
    /// Mailpit, a mail catcher: SMTP on localhost:1025, inbox on http://localhost:8025.
    Mailpit,
    /// MongoDB 8 on localhost:27017.
    Mongodb,
    /// RabbitMQ with its management page: AMQP on localhost:5672, UI on http://localhost:15672 (guest / guest).
    Rabbitmq,
    /// NATS with JetStream on localhost:4222 (monitoring on http://localhost:8222).
    Nats,
    /// Grafana on http://localhost:3000 (admin / admin).
    Grafana,
    /// Redpanda, a Kafka-compatible broker: Kafka API on localhost:9092, admin API on http://localhost:9644.
    Redpanda,
    /// Qdrant vector database: REST on http://localhost:6333 (dashboard at /dashboard), gRPC on localhost:6334.
    Qdrant,
    /// Meilisearch on http://localhost:7700 (development mode: no key, built-in search page).
    Meilisearch,
    /// ClickHouse: HTTP on http://localhost:8123, native protocol on localhost:9000 (user default, password dev).
    Clickhouse,
    /// Keycloak identity server in dev mode on http://localhost:8180 (admin / admin). Needs about 1 GiB of VM memory.
    Keycloak,
    /// SeaweedFS, S3-compatible storage: S3 API on http://localhost:8333 (no credentials), master page on http://localhost:9333.
    Seaweedfs,
    /// HashiCorp Vault in dev mode on http://localhost:8200 (root token dev). Secrets live in memory only.
    Vault,
    /// Jaeger tracing: UI on http://localhost:16686, OTLP on localhost:4317 (gRPC) and localhost:4318 (HTTP).
    Jaeger,
    /// Temporal dev server: gRPC on localhost:7233, web UI on http://localhost:8233.
    Temporal,
}

pub const ALL: [App; 17] = [
    App::Postgres,
    App::Redis,
    App::Mysql,
    App::Mailpit,
    App::Mongodb,
    App::Rabbitmq,
    App::Nats,
    App::Grafana,
    App::Redpanda,
    App::Qdrant,
    App::Meilisearch,
    App::Clickhouse,
    App::Keycloak,
    App::Seaweedfs,
    App::Vault,
    App::Jaeger,
    App::Temporal,
];

struct Port {
    name: &'static str,
    container: u16,
    node: u16,
    local: u16,
}

struct Spec {
    image: &'static str,
    env: &'static [(&'static str, &'static str)],
    args: &'static [&'static str],
    ports: &'static [Port],
    /// where the data lives, and how much room it gets; `None` keeps nothing
    volume: Option<(&'static str, &'static str)>,
}

impl App {
    pub fn name(self) -> &'static str {
        match self {
            App::Postgres => "postgres",
            App::Redis => "redis",
            App::Mysql => "mysql",
            App::Mailpit => "mailpit",
            App::Mongodb => "mongodb",
            App::Rabbitmq => "rabbitmq",
            App::Nats => "nats",
            App::Grafana => "grafana",
            App::Redpanda => "redpanda",
            App::Qdrant => "qdrant",
            App::Meilisearch => "meilisearch",
            App::Clickhouse => "clickhouse",
            App::Keycloak => "keycloak",
            App::Seaweedfs => "seaweedfs",
            App::Vault => "vault",
            App::Jaeger => "jaeger",
            App::Temporal => "temporal",
        }
    }

    /// The command that opens a shell on this app inside its pod (`fluxctl kube connect`), if it has one.
    pub fn shell(self) -> Option<&'static str> {
        match self {
            App::Postgres => Some("psql -U postgres"),
            App::Redis => Some("redis-cli"),
            App::Mysql => Some("mysql -uroot -pdev app"),
            App::Mongodb => Some("mongosh"),
            App::Clickhouse => Some("clickhouse-client --password dev"),
            _ => None,
        }
    }

    fn spec(self) -> Spec {
        match self {
            App::Postgres => Spec {
                image: "docker.io/library/postgres:17-alpine",
                env: &[
                    ("POSTGRES_PASSWORD", "dev"),
                    ("PGDATA", "/var/lib/postgresql/data/pgdata"),
                ],
                args: &[],
                ports: &[Port {
                    name: "pg",
                    container: 5432,
                    node: 30432,
                    local: 5432,
                }],
                volume: Some(("/var/lib/postgresql/data", "2Gi")),
            },
            App::Redis => Spec {
                image: "docker.io/library/redis:7-alpine",
                env: &[],
                // protected mode would refuse the forwarded connection (it does not come from loopback inside the VM)
                args: &[
                    "redis-server",
                    "--appendonly",
                    "yes",
                    "--protected-mode",
                    "no",
                ],
                ports: &[Port {
                    name: "redis",
                    container: 6379,
                    node: 30379,
                    local: 6379,
                }],
                volume: Some(("/data", "1Gi")),
            },
            App::Mysql => Spec {
                image: "docker.io/library/mysql:8.4",
                env: &[("MYSQL_ROOT_PASSWORD", "dev"), ("MYSQL_DATABASE", "app")],
                args: &[],
                ports: &[Port {
                    name: "mysql",
                    container: 3306,
                    node: 30306,
                    local: 3306,
                }],
                volume: Some(("/var/lib/mysql", "2Gi")),
            },
            // The inbox has the lower node port, so `http://mailpit.localhost` opens it.
            App::Mailpit => Spec {
                image: "docker.io/axllent/mailpit:latest",
                env: &[],
                args: &[],
                ports: &[
                    Port {
                        name: "http",
                        container: 8025,
                        node: 30825,
                        local: 8025,
                    },
                    Port {
                        name: "smtp",
                        container: 1025,
                        node: 31025,
                        local: 1025,
                    },
                ],
                volume: None,
            },
            App::Mongodb => Spec {
                image: "docker.io/library/mongo:8",
                env: &[],
                args: &[],
                ports: &[Port {
                    name: "mongo",
                    container: 27017,
                    node: 30017,
                    local: 27017,
                }],
                volume: Some(("/data/db", "2Gi")),
            },
            // The management page has the lower node port, so `http://rabbitmq.localhost` opens it.
            App::Rabbitmq => Spec {
                image: "docker.io/library/rabbitmq:4-management-alpine",
                env: &[],
                args: &[],
                ports: &[
                    Port {
                        name: "management",
                        container: 15672,
                        node: 30567,
                        local: 15672,
                    },
                    Port {
                        name: "amqp",
                        container: 5672,
                        node: 30568,
                        local: 5672,
                    },
                ],
                volume: Some(("/var/lib/rabbitmq", "1Gi")),
            },
            // The monitoring page has the lower node port, so `http://nats.localhost` opens it.
            App::Nats => Spec {
                image: "docker.io/library/nats:2-alpine",
                env: &[],
                args: &["-js", "-m", "8222"],
                ports: &[
                    Port {
                        name: "monitor",
                        container: 8222,
                        node: 30222,
                        local: 8222,
                    },
                    Port {
                        name: "client",
                        container: 4222,
                        node: 30223,
                        local: 4222,
                    },
                ],
                volume: None,
            },
            App::Grafana => Spec {
                image: "docker.io/grafana/grafana:11.4.0",
                env: &[],
                args: &[],
                ports: &[Port {
                    name: "http",
                    container: 3000,
                    node: 30300,
                    local: 3000,
                }],
                volume: Some(("/var/lib/grafana", "1Gi")),
            },
            // The Kafka API advertises 127.0.0.1:9092, which is what a client on the Mac dials, so it must stay on 9092.
            App::Redpanda => Spec {
                image: "docker.io/redpandadata/redpanda:v24.3.1",
                env: &[],
                args: &[
                    "redpanda",
                    "start",
                    "--overprovisioned",
                    "--smp",
                    "1",
                    "--memory",
                    "768M",
                    "--reserve-memory",
                    "0M",
                    "--node-id",
                    "0",
                    "--check=false",
                    "--kafka-addr",
                    "0.0.0.0:9092",
                    "--advertise-kafka-addr",
                    "127.0.0.1:9092",
                ],
                ports: &[
                    Port {
                        name: "admin",
                        container: 9644,
                        node: 30090,
                        local: 9644,
                    },
                    Port {
                        name: "kafka",
                        container: 9092,
                        node: 30092,
                        local: 9092,
                    },
                ],
                volume: Some(("/var/lib/redpanda/data", "2Gi")),
            },
            App::Qdrant => Spec {
                image: "docker.io/qdrant/qdrant:v1.12.4",
                env: &[],
                args: &[],
                ports: &[
                    Port {
                        name: "rest",
                        container: 6333,
                        node: 30633,
                        local: 6333,
                    },
                    Port {
                        name: "grpc",
                        container: 6334,
                        node: 30634,
                        local: 6334,
                    },
                ],
                volume: Some(("/qdrant/storage", "2Gi")),
            },
            App::Meilisearch => Spec {
                image: "docker.io/getmeili/meilisearch:v1.12",
                env: &[("MEILI_ENV", "development")],
                args: &[],
                ports: &[Port {
                    name: "http",
                    container: 7700,
                    node: 30770,
                    local: 7700,
                }],
                volume: Some(("/meili_data", "1Gi")),
            },
            App::Clickhouse => Spec {
                image: "docker.io/clickhouse/clickhouse-server:24.12-alpine",
                env: &[
                    ("CLICKHOUSE_USER", "default"),
                    ("CLICKHOUSE_PASSWORD", "dev"),
                    ("CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT", "1"),
                ],
                args: &[],
                ports: &[
                    Port {
                        name: "http",
                        container: 8123,
                        node: 30123,
                        local: 8123,
                    },
                    Port {
                        name: "native",
                        container: 9000,
                        node: 30900,
                        local: 9000,
                    },
                ],
                volume: Some(("/var/lib/clickhouse", "2Gi")),
            },
            // Starts the dev server (H2 file database); the first start takes a minute.
            App::Keycloak => Spec {
                image: "quay.io/keycloak/keycloak:26.0",
                env: &[
                    ("KC_BOOTSTRAP_ADMIN_USERNAME", "admin"),
                    ("KC_BOOTSTRAP_ADMIN_PASSWORD", "admin"),
                ],
                args: &["start-dev"],
                ports: &[Port {
                    name: "http",
                    container: 8080,
                    node: 30818,
                    local: 8180,
                }],
                volume: Some(("/opt/keycloak/data", "1Gi")),
            },
            // The master page has the lower node port, so `http://seaweedfs.localhost` opens it. One process serves the
            // master, a volume server and the S3 gateway; without an identities file S3 accepts any credentials.
            App::Seaweedfs => Spec {
                image: "docker.io/chrislusf/seaweedfs:4.00",
                env: &[],
                args: &["server", "-s3", "-dir=/data", "-ip.bind=0.0.0.0"],
                ports: &[
                    Port {
                        name: "master",
                        container: 9333,
                        node: 30933,
                        local: 9333,
                    },
                    Port {
                        name: "s3",
                        container: 8333,
                        node: 31833,
                        local: 8333,
                    },
                ],
                volume: Some(("/data", "2Gi")),
            },
            // Dev mode keeps everything in memory and starts unsealed with a fixed root token.
            App::Vault => Spec {
                image: "docker.io/hashicorp/vault:1.18",
                env: &[
                    ("VAULT_DEV_ROOT_TOKEN_ID", "dev"),
                    ("VAULT_DEV_LISTEN_ADDRESS", "0.0.0.0:8200"),
                ],
                args: &["server", "-dev"],
                ports: &[Port {
                    name: "http",
                    container: 8200,
                    node: 30820,
                    local: 8200,
                }],
                volume: None,
            },
            // The UI has the lowest node port, so `http://jaeger.localhost` opens it. Traces are kept in memory.
            App::Jaeger => Spec {
                image: "docker.io/jaegertracing/all-in-one:1.64.0",
                env: &[("COLLECTOR_OTLP_ENABLED", "true")],
                args: &[],
                ports: &[
                    Port {
                        name: "ui",
                        container: 16686,
                        node: 30686,
                        local: 16686,
                    },
                    Port {
                        name: "otlp-grpc",
                        container: 4317,
                        node: 31431,
                        local: 4317,
                    },
                    Port {
                        name: "otlp-http",
                        container: 4318,
                        node: 31418,
                        local: 4318,
                    },
                ],
                volume: None,
            },
            // The web UI has the lower node port, so `http://temporal.localhost` opens it.
            App::Temporal => Spec {
                image: "docker.io/temporalio/temporal:latest",
                env: &[],
                args: &[
                    "server",
                    "start-dev",
                    "--ip",
                    "0.0.0.0",
                    "--db-filename",
                    "/data/temporal.db",
                ],
                ports: &[
                    Port {
                        name: "ui",
                        container: 8233,
                        node: 30233,
                        local: 8233,
                    },
                    Port {
                        name: "grpc",
                        container: 7233,
                        node: 30723,
                        local: 7233,
                    },
                ],
                volume: Some(("/data", "1Gi")),
            },
        }
    }

    /// `connect()` with the ports the app really got on the Mac.
    pub fn connect_with(self, locals: &[u16]) -> Vec<(&'static str, String)> {
        let usual: Vec<u16> = self.spec().ports.iter().map(|p| p.local).collect();
        self.connect()
            .into_iter()
            .map(|(k, mut v)| {
                for (old, new) in usual.iter().zip(locals) {
                    if old != new {
                        v = v
                            .replace(&format!(":{old}"), &format!(":{new}"))
                            .replace(&format!("-p {old}"), &format!("-p {new}"));
                    }
                }
                (k, v)
            })
            .collect()
    }

    /// What to paste into an application: (label, value).
    pub fn connect(self) -> Vec<(&'static str, String)> {
        match self {
            App::Postgres => vec![
                (
                    "url",
                    "postgresql://postgres:dev@127.0.0.1:5432/postgres".into(),
                ),
                (
                    "psql",
                    "psql postgresql://postgres:dev@127.0.0.1:5432/postgres".into(),
                ),
            ],
            App::Redis => vec![
                ("url", "redis://127.0.0.1:6379".into()),
                ("cli", "redis-cli -p 6379".into()),
            ],
            App::Mysql => vec![
                ("url", "mysql://root:dev@127.0.0.1:3306/app".into()),
                ("cli", "mysql -h 127.0.0.1 -u root -pdev app".into()),
            ],
            App::Mailpit => vec![
                ("smtp", "127.0.0.1:1025 (no auth, no TLS)".into()),
                (
                    "inbox",
                    "http://127.0.0.1:8025 or http://mailpit.localhost".into(),
                ),
            ],
            App::Mongodb => vec![
                ("url", "mongodb://127.0.0.1:27017".into()),
                (
                    "shell",
                    "mongosh (or `fluxctl kube connect mongodb`)".into(),
                ),
            ],
            App::Rabbitmq => vec![
                ("amqp", "amqp://guest:guest@127.0.0.1:5672/".into()),
                (
                    "management",
                    "http://127.0.0.1:15672 or http://rabbitmq.localhost (guest / guest)".into(),
                ),
            ],
            App::Nats => vec![
                ("url", "nats://127.0.0.1:4222".into()),
                (
                    "monitoring",
                    "http://127.0.0.1:8222 or http://nats.localhost".into(),
                ),
            ],
            App::Grafana => vec![
                (
                    "ui",
                    "http://127.0.0.1:3000 or http://grafana.localhost".into(),
                ),
                ("login", "admin / admin".into()),
            ],
            App::Redpanda => vec![
                ("kafka", "127.0.0.1:9092 (bootstrap server)".into()),
                (
                    "admin",
                    "http://127.0.0.1:9644 or http://redpanda.localhost".into(),
                ),
            ],
            App::Qdrant => vec![
                (
                    "rest",
                    "http://127.0.0.1:6333 or http://qdrant.localhost".into(),
                ),
                ("dashboard", "http://127.0.0.1:6333/dashboard".into()),
                ("grpc", "127.0.0.1:6334".into()),
            ],
            App::Meilisearch => vec![
                (
                    "ui",
                    "http://127.0.0.1:7700 or http://meilisearch.localhost".into(),
                ),
                ("key", "none (development mode)".into()),
            ],
            App::Clickhouse => vec![
                ("http", "http://default:dev@127.0.0.1:8123".into()),
                (
                    "cli",
                    "clickhouse-client --host 127.0.0.1 --port 9000 --password dev".into(),
                ),
            ],
            App::Keycloak => vec![
                (
                    "ui",
                    "http://127.0.0.1:8180 or http://keycloak.localhost".into(),
                ),
                ("login", "admin / admin".into()),
            ],
            App::Seaweedfs => vec![
                (
                    "master",
                    "http://127.0.0.1:9333 or http://seaweedfs.localhost".into(),
                ),
                (
                    "s3",
                    "http://127.0.0.1:8333 (path-style addressing; any access key works)".into(),
                ),
            ],
            App::Vault => vec![
                (
                    "ui",
                    "http://127.0.0.1:8200 or http://vault.localhost".into(),
                ),
                (
                    "cli",
                    "VAULT_ADDR=http://127.0.0.1:8200 VAULT_TOKEN=dev vault status".into(),
                ),
            ],
            App::Jaeger => vec![
                (
                    "ui",
                    "http://127.0.0.1:16686 or http://jaeger.localhost".into(),
                ),
                (
                    "otlp",
                    "grpc 127.0.0.1:4317, http http://127.0.0.1:4318".into(),
                ),
            ],
            App::Temporal => vec![
                (
                    "ui",
                    "http://127.0.0.1:8233 or http://temporal.localhost".into(),
                ),
                ("grpc", "127.0.0.1:7233".into()),
            ],
        }
    }

    /// The Mac ports this app answers on.
    pub fn local_ports(self) -> Vec<(u16, u16)> {
        self.spec()
            .ports
            .iter()
            .map(|p| (p.local, p.node))
            .collect()
    }

    /// The Deployment, Service and (when it keeps data) PersistentVolumeClaim, for namespace `default`, with the usual
    /// ports on the Mac.
    pub fn manifest(self) -> String {
        self.manifest_with(
            &self
                .spec()
                .ports
                .iter()
                .map(|p| p.local)
                .collect::<Vec<_>>(),
        )
    }

    /// The same, answering on these Mac ports (one per port of the app, in order) instead of the usual ones.
    pub fn manifest_with(self, locals: &[u16]) -> String {
        let (n, sp) = (self.name(), self.spec());
        let labels =
            format!("app.kubernetes.io/name: {n}\n    app.kubernetes.io/managed-by: fluxvm");
        let mut y = String::new();
        if let Some((_, size)) = sp.volume {
            y.push_str(&format!(
                "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: {n}-data\n  namespace: default\n  labels:\n    {labels}\nspec:\n  accessModes: [ReadWriteOnce]\n  resources:\n    requests:\n      storage: {size}\n---\n"
            ));
        }
        let env: String = sp
            .env
            .iter()
            .map(|(k, v)| format!("        - {{name: {k}, value: \"{v}\"}}\n"))
            .collect();
        let args: String = sp
            .args
            .iter()
            .map(|a| format!("        - \"{a}\"\n"))
            .collect();
        let ports: String = sp
            .ports
            .iter()
            .map(|p| {
                format!(
                    "        - {{name: {}, containerPort: {}}}\n",
                    p.name, p.container
                )
            })
            .collect();
        let volume_mount = sp.volume.map_or(String::new(), |(path, _)| {
            format!("        volumeMounts:\n        - {{name: data, mountPath: {path}}}\n")
        });
        let volume = match sp.volume {
            Some(_) => format!(
                "      volumes:\n      - name: data\n        persistentVolumeClaim:\n          claimName: {n}-data\n"
            ),
            None => "      volumes:\n      - name: data\n        emptyDir: {}\n".to_owned(),
        };
        y.push_str(&format!(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {n}\n  namespace: default\n  labels:\n    {labels}\nspec:\n  replicas: 1\n  strategy:\n    type: Recreate\n  selector:\n    matchLabels:\n      app.kubernetes.io/name: {n}\n  template:\n    metadata:\n      labels:\n        {tl}\n    spec:\n      containers:\n      - name: {n}\n        image: {image}\n{args_block}{env_block}        ports:\n{ports}        readinessProbe:\n          tcpSocket:\n            port: {first}\n          periodSeconds: 3\n{volume_mount}{volume}---\n",
            tl = "app.kubernetes.io/name: ".to_owned() + n,
            image = sp.image,
            args_block = if args.is_empty() { String::new() } else { format!("        args:\n{args}") },
            env_block = if env.is_empty() { String::new() } else { format!("        env:\n{env}") },
            first = sp.ports[0].container,
        ));
        let annotation = sp
            .ports
            .iter()
            .zip(locals)
            .map(|(p, l)| format!("{}={}", p.node, l))
            .collect::<Vec<_>>()
            .join(",");
        let svc_ports: String = sp
            .ports
            .iter()
            .map(|p| {
                format!(
                    "  - {{name: {}, port: {}, targetPort: {}, nodePort: {}}}\n",
                    p.name, p.container, p.container, p.node
                )
            })
            .collect();
        y.push_str(&format!(
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: {n}\n  namespace: default\n  labels:\n    {labels}\n  annotations:\n    fluxvm.dev/local-ports: \"{annotation}\"\nspec:\n  type: NodePort\n  selector:\n    app.kubernetes.io/name: {n}\n  ports:\n{svc_ports}"
        ));
        y
    }
}

/// The Mac port each of the app's ports gets: its usual one, or the next free one when `busy` says something already uses it
/// (a Homebrew Postgres on 5432, say).
pub(crate) fn pick_ports(app: App, busy: impl Fn(u16) -> bool) -> Vec<u16> {
    let mut chosen: Vec<u16> = vec![];
    for p in app.spec().ports {
        let mut port = p.local;
        while (busy(port) || chosen.contains(&port)) && port < p.local.saturating_add(50) {
            port += 1;
        }
        chosen.push(port);
    }
    chosen
}

/// `fluxctl kube add APP`: applies the manifest in the cluster, waits until it is ready, opens its ports on this Mac and
/// returns the lines to print.
pub async fn add<A: VmApi>(api: &A, o: &KubeOptions, app: App) -> Result<String> {
    // A port our own forwarder already holds (a second `add`) is not "in use by something else".
    let held = kube_host::forwarded_hosts(o);
    let locals = pick_ports(app, |p| kube_host::port_in_use(p) && !held.contains(&p));
    kube_host::guest_apply(api, o, &app.manifest_with(&locals)).await?;
    eprintln!(
        "started {}, waiting until it is ready (the first start pulls its image)…",
        app.name()
    );
    if let Err(e) = kube_host::guest_sh(
        api,
        o,
        &format!(
            "sudo k3s kubectl -n default rollout status deploy/{} --timeout=600s",
            app.name()
        ),
    )
    .await
    {
        let why = kube_host::guest_sh(
            api,
            o,
            &format!(
                "sudo k3s kubectl -n default get pods -l app.kubernetes.io/name={} -o jsonpath='{{range .items[*]}}{{.status.containerStatuses[*].state.waiting.reason}}: {{.status.containerStatuses[*].state.waiting.message}}{{\"\\n\"}}{{end}}'",
                app.name()
            ),
        )
        .await
        .unwrap_or_default();
        bail!(
            "{} did not become ready: {}{}",
            app.name(),
            e,
            if why.trim().is_empty() {
                String::new()
            } else {
                format!("\n  the pod says: {}", why.trim())
            }
        );
    }
    let nodes: Vec<u16> = app.local_ports().iter().map(|(_, n)| *n).collect();
    let published = match kube_host::publish_now(api, o).await {
        Ok(()) => locals
            .iter()
            .zip(&nodes)
            .all(|(h, g)| kube_host::is_forwarded(o, *h, *g)),
        Err(_) => false,
    };
    let mut unserved = vec![];
    if published {
        for host in &locals {
            if !wait_serving(*host, std::time::Duration::from_secs(90)).await {
                unserved.push(*host);
            }
        }
    }
    let mut out = format!(
        "{} is running in the cluster (namespace default)\n",
        app.name()
    );
    let usual: Vec<u16> = app.local_ports().iter().map(|(l, _)| *l).collect();
    for (u, l) in usual.iter().zip(&locals) {
        if u != l {
            out.push_str(&format!(
                "  port {u} is in use on this Mac, so it is on {l} instead\n"
            ));
        }
    }
    if published && !unserved.is_empty() {
        out.push_str(&format!(
            "  but nothing answered on 127.0.0.1:{} after 90 s (the forward is open and the pod is ready; look at `kubectl logs deploy/{}`)\n",
            unserved.iter().map(u16::to_string).collect::<Vec<_>>().join(", "),
            app.name()
        ));
    } else if published {
        for (k, v) in app.connect_with(&locals) {
            out.push_str(&format!("  {k:<12} {v}\n"));
        }
        out.push_str("  keep the ports open across restarts with `fluxctl kube agent install`\n");
    } else {
        out.push_str(&format!(
            "  but the ports on 127.0.0.1 could not be opened. It is still reachable inside the cluster as {}.default.svc.\n",
            app.name()
        ));
    }
    Ok(out)
}

/// `fluxctl kube connect APP`: a database shell inside the pod, so a Mac with no `psql` or `redis-cli` still has one.
pub async fn connect<A: VmApi>(api: &A, o: &KubeOptions, app: App) -> Result<i32> {
    let Some(shell) = app.shell() else {
        bail!(
            "{} has no shell to open. Its address:\n{}",
            app.name(),
            app.connect()
                .iter()
                .map(|(k, v)| format!("  {k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    };
    kube_host::guest_interactive(
        api,
        o,
        &format!(
            "sudo k3s kubectl -n default exec -it deploy/{} -- {shell}",
            app.name()
        ),
    )
    .await
}

/// The URL `fluxctl kube open NAME` opens: the name on port 80 when that is being served, otherwise the service's port on
/// 127.0.0.1. `NAME` is a service name, or `namespace/name`.
pub(crate) fn pick_url(
    services: &[kube_host::NodePortService],
    name: &str,
    names_up: bool,
) -> Result<String> {
    let (ns, svc) = name.split_once('/').unwrap_or(("", name));
    let mut found: Vec<&kube_host::NodePortService> = services
        .iter()
        .filter(|s| s.name == svc && (ns.is_empty() || s.namespace == ns))
        .collect();
    found.sort_by_key(|s| s.port);
    let Some(s) = found.first() else {
        let known: Vec<String> = services.iter().map(|s| s.name.clone()).collect();
        bail!(
            "no published service named {name:?}. Published: {}",
            if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            }
        );
    };
    Ok(if names_up {
        if s.namespace == "default" {
            format!("http://{}.localhost", s.name)
        } else {
            format!("http://{}.{}.localhost", s.name, s.namespace)
        }
    } else {
        format!("http://127.0.0.1:{}", s.host_port())
    })
}

/// Whether a TCP service really answers on 127.0.0.1:`port`. An open forward to a dead backend accepts and then closes
/// at once, so a connection that stays open for a moment (a server waiting for us) or that gets a greeting counts.
pub(crate) fn serving(port: u16) -> bool {
    use std::io::Read;
    let Ok(mut c) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(2),
    ) else {
        return false;
    };
    let _ = c.set_read_timeout(Some(std::time::Duration::from_millis(600)));
    let mut b = [0u8; 1];
    match c.read(&mut b) {
        Ok(0) => false, // closed on us
        Ok(_) => true,  // greeted us first (SMTP, MySQL)
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

pub(crate) async fn wait_serving(port: u16, within: std::time::Duration) -> bool {
    let end = std::time::Instant::now() + within;
    loop {
        if tokio::task::spawn_blocking(move || serving(port))
            .await
            .unwrap_or(false)
        {
            return true;
        }
        if std::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// `fluxctl kube remove APP`: stops it; `delete_data` also erases the volume.
pub async fn remove<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    app: App,
    delete_data: bool,
) -> Result<String> {
    let sel = format!(
        "-l app.kubernetes.io/name={},app.kubernetes.io/managed-by=fluxvm",
        app.name()
    );
    let kinds = if delete_data {
        "deploy,svc,pvc"
    } else {
        "deploy,svc"
    };
    let out = kube_host::guest_sh(
        api,
        o,
        &format!("sudo k3s kubectl -n default delete {kinds} {sel} --ignore-not-found"),
    )
    .await?;
    if out.is_empty() {
        bail!("{} is not installed", app.name());
    }
    Ok(format!(
        "{}{}",
        out,
        if delete_data {
            "\n(its data is deleted)"
        } else {
            "\n(its data is kept; add it again to get it back, or pass --delete-data)"
        }
    ))
}

/// `fluxctl kube apps`: what can be added, and what is running.
pub async fn list<A: VmApi>(api: &A, o: &KubeOptions) -> Result<String> {
    let running = kube_host::guest_sh(
        api,
        o,
        "sudo k3s kubectl -n default get deploy -l app.kubernetes.io/managed-by=fluxvm -o jsonpath='{.items[*].metadata.name}'",
    )
    .await
    .unwrap_or_default();
    let mut out = String::new();
    for a in ALL {
        let on = running.split_whitespace().any(|r| r == a.name());
        let ports: Vec<String> = a.local_ports().iter().map(|(h, _)| h.to_string()).collect();
        out.push_str(&format!(
            "{:<12} {:<8} localhost:{}\n",
            a.name(),
            if on { "running" } else { "-" },
            ports.join(", ")
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_apps_fight_over_a_port() {
        let mut nodes = vec![];
        let mut locals = vec![];
        for a in ALL {
            for (l, n) in a.local_ports() {
                assert!((30000..=32767).contains(&n), "{} node port {n}", a.name());
                assert!(l >= 1024, "{} local port {l}", a.name());
                nodes.push(n);
                locals.push(l);
            }
        }
        let (mut n2, mut l2) = (nodes.clone(), locals.clone());
        n2.sort();
        n2.dedup();
        l2.sort();
        l2.dedup();
        assert_eq!((n2.len(), l2.len()), (nodes.len(), locals.len()));
    }

    #[test]
    fn the_manifest_is_valid_yaml_and_says_which_ports_to_open_on_the_mac() {
        for a in ALL {
            let text = a.manifest();
            let docs: Vec<serde_yaml::Value> = text
                .split("---\n")
                .filter(|d| !d.trim().is_empty())
                .map(|d| {
                    serde_yaml::from_str(d).unwrap_or_else(|e| panic!("{}: {e}\n{d}", a.name()))
                })
                .collect();
            let kinds: Vec<&str> = docs.iter().filter_map(|d| d["kind"].as_str()).collect();
            assert!(
                kinds.contains(&"Deployment") && kinds.contains(&"Service"),
                "{kinds:?}"
            );
            assert_eq!(
                kinds.contains(&"PersistentVolumeClaim"),
                a.spec().volume.is_some()
            );
            let svc = docs.iter().find(|d| d["kind"] == "Service").unwrap();
            assert_eq!(svc["spec"]["type"].as_str(), Some("NodePort"));
            let ann = svc["metadata"]["annotations"]["fluxvm.dev/local-ports"]
                .as_str()
                .unwrap();
            let mut got = kube_host::parse_local_ports(ann);
            let mut want: Vec<(u16, u16)> = a.local_ports().iter().map(|(l, n)| (*n, *l)).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "{}", a.name());
            let dep = docs.iter().find(|d| d["kind"] == "Deployment").unwrap();
            assert_eq!(
                dep["spec"]["strategy"]["type"].as_str(),
                Some("Recreate"),
                "one volume, one pod at a time"
            );
        }
    }

    #[test]
    fn open_picks_a_name_when_names_are_served_and_a_port_when_not() {
        let svc =
            |ns: &str, name: &str, port: u16, local: Option<u16>| kube_host::NodePortService {
                namespace: ns.into(),
                name: name.into(),
                port,
                local,
                service_port: 0,
            };
        let all = [
            svc("default", "mailpit", 31025, Some(1025)),
            svc("default", "mailpit", 30825, Some(8025)),
            svc("shop", "web", 30080, None),
        ];
        assert_eq!(
            pick_url(&all, "mailpit", true).unwrap(),
            "http://mailpit.localhost"
        );
        assert_eq!(
            pick_url(&all, "mailpit", false).unwrap(),
            "http://127.0.0.1:8025",
            "the lowest node port is the inbox"
        );
        assert_eq!(
            pick_url(&all, "shop/web", true).unwrap(),
            "http://web.shop.localhost"
        );
        assert_eq!(
            pick_url(&all, "web", false).unwrap(),
            "http://127.0.0.1:30080"
        );
        let e = pick_url(&all, "nope", true).unwrap_err().to_string();
        assert!(e.contains("mailpit") && e.contains("web"), "{e}");
    }

    #[test]
    fn a_port_counts_as_serving_when_the_server_waits_or_greets_and_not_when_it_hangs_up() {
        use std::{io::Write, net::TcpListener, thread};
        let waits = TcpListener::bind("127.0.0.1:0").unwrap();
        let p1 = waits.local_addr().unwrap().port();
        let t1 = thread::spawn(move || {
            let (c, _) = waits.accept().unwrap();
            thread::sleep(std::time::Duration::from_millis(900));
            drop(c);
        });
        assert!(serving(p1), "a server that waits for us is serving");
        t1.join().unwrap();
        let hangs_up = TcpListener::bind("127.0.0.1:0").unwrap();
        let p2 = hangs_up.local_addr().unwrap().port();
        let t2 = thread::spawn(move || drop(hangs_up.accept().unwrap().0));
        assert!(
            !serving(p2),
            "an open forward to a dead backend closes at once"
        );
        t2.join().unwrap();
        let greets = TcpListener::bind("127.0.0.1:0").unwrap();
        let p3 = greets.local_addr().unwrap().port();
        let t3 = thread::spawn(move || {
            let (mut c, _) = greets.accept().unwrap();
            c.write_all(b"220 hello\r\n").unwrap();
            thread::sleep(std::time::Duration::from_millis(200));
        });
        assert!(serving(p3), "a server that greets first is serving");
        t3.join().unwrap();
    }

    #[test]
    fn a_busy_mac_port_moves_the_app_to_the_next_free_one_and_the_connection_line_follows() {
        assert_eq!(pick_ports(App::Postgres, |_| false), vec![5432]);
        let locals = pick_ports(App::Postgres, |p| p == 5432 || p == 5433);
        assert_eq!(locals, vec![5434]);
        assert!(
            App::Postgres.connect_with(&locals)[0]
                .1
                .contains("127.0.0.1:5434/postgres")
        );
        assert!(App::Postgres.manifest_with(&locals).contains("30432=5434"));
        assert!(
            App::Redis
                .connect_with(&[6380])
                .iter()
                .any(|(_, v)| v == "redis-cli -p 6380")
        );
        // two ports of one app never land on the same Mac port
        let two = pick_ports(App::Rabbitmq, |p| p == 15672);
        assert_eq!(two[0], 15673);
        assert!(two[0] != two[1]);
        // the usual ports leave the manifest as it was
        assert_eq!(App::Mysql.manifest(), App::Mysql.manifest_with(&[3306]));
    }

    #[test]
    fn only_databases_have_a_shell() {
        assert_eq!(App::Postgres.shell(), Some("psql -U postgres"));
        assert!(App::Rabbitmq.shell().is_none() && App::Grafana.shell().is_none());
        for a in ALL {
            if let Some(sh) = a.shell() {
                assert!(!sh.contains(['\'', ';', '|']), "{sh}");
            }
        }
    }

    #[test]
    fn the_connection_lines_use_the_port_the_app_gets_on_the_mac() {
        assert!(App::Postgres.connect()[0].1.contains(":5432/"));
        assert!(App::Redis.connect()[0].1.ends_with(":6379"));
        assert!(
            App::Mailpit
                .connect()
                .iter()
                .any(|(_, v)| v.contains("8025"))
        );
        assert_eq!(App::Mysql.local_ports(), vec![(3306, 30306)]);
    }
}
