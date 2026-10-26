//! `fluxctl kube db-backup postgres` and `db-restore`: a database's contents as one file on the Mac, so a cluster can be
//! thrown away and rebuilt without losing the data you were developing against. The dump runs inside the pod with the
//! database's own tool; nothing needs installing on the Mac.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::kube_apps::App;
use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

/// The in-pod commands for an app's dump and restore, and the file extension, or `None` for apps that are not databases.
#[derive(Debug)]
pub struct Tools {
    pub dump: &'static str,
    pub restore: &'static str,
    pub ext: &'static str,
}

pub fn tools(app: App) -> Option<Tools> {
    match app {
        App::Postgres => Some(Tools {
            dump: "pg_dumpall -U postgres --clean --if-exists",
            restore: "psql -U postgres -q",
            ext: "sql",
        }),
        App::Mysql => Some(Tools {
            dump: "mysqldump -uroot -pdev --all-databases --single-transaction --routines --events",
            restore: "mysql -uroot -pdev",
            ext: "sql",
        }),
        App::Mongodb => Some(Tools {
            dump: "mongodump --archive --quiet",
            restore: "mongorestore --archive --drop --quiet",
            ext: "archive",
        }),
        _ => None,
    }
}

fn need_tools(app: App) -> Result<Tools> {
    tools(app).with_context(|| {
        format!(
            "{} has no backup yet; `db-backup` works for postgres, mysql and mongodb (a cluster checkpoint saves everything)",
            app.name()
        )
    })
}

/// The script run in the VM: the tool inside the app's pod, reading stdin and writing stdout.
pub fn exec_script(app: App, tool: &str) -> String {
    // `tool` is one of the constants above, never user input, so it needs no escaping inside the single quotes.
    format!(
        "sudo k3s kubectl exec -i -n default deploy/{} -- sh -c '{tool}'",
        app.name()
    )
}

/// `postgres-20261026-101500.sql`, in the current directory.
pub fn default_file(app: App, stamp: &str) -> PathBuf {
    let ext = tools(app).map_or("dump", |t| t.ext);
    PathBuf::from(format!("{}-{stamp}.{ext}", app.name()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    // A dump holds the data itself, so it is readable by its owner only, and an existing file is never overwritten.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {} (it must not exist)", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

pub async fn backup<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    app: App,
    file: Option<&Path>,
) -> Result<String> {
    let t = need_tools(app)?;
    let path = match file {
        Some(f) => f.to_owned(),
        None => default_file(
            app,
            &chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
        ),
    };
    if path.exists() {
        bail!("{} already exists; pick another name", path.display());
    }
    let dump = kube_host::guest_output_bytes(api, o, &exec_script(app, t.dump), None)
        .await
        .with_context(|| {
            format!(
                "dumping {} (is it running? `fluxctl kube apps`)",
                app.name()
            )
        })?;
    if dump.is_empty() {
        bail!("{} produced an empty dump", app.name());
    }
    write_private(&path, &dump)?;
    Ok(format!(
        "{}: {} KiB -> {}",
        app.name(),
        dump.len().div_ceil(1024),
        path.display()
    ))
}

pub async fn restore<A: VmApi>(api: &A, o: &KubeOptions, app: App, file: &Path) -> Result<String> {
    let t = need_tools(app)?;
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    if data.is_empty() {
        bail!("{} is empty", file.display());
    }
    kube_host::guest_output_bytes(api, o, &exec_script(app, t.restore), Some(&data))
        .await
        .with_context(|| {
            format!(
                "restoring into {} (is it running? `fluxctl kube apps`)",
                app.name()
            )
        })?;
    Ok(format!(
        "{}: restored {} KiB from {}",
        app.name(),
        data.len().div_ceil(1024),
        file.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_databases_have_a_backup() {
        for a in [App::Postgres, App::Mysql, App::Mongodb] {
            assert!(tools(a).is_some(), "{}", a.name());
        }
        for a in [
            App::Redis,
            App::Mailpit,
            App::Grafana,
            App::Nats,
            App::Qdrant,
        ] {
            assert!(tools(a).is_none(), "{}", a.name());
            assert!(need_tools(a).unwrap_err().to_string().contains("no backup"));
        }
    }

    #[test]
    fn the_script_runs_the_tool_in_the_apps_pod() {
        assert_eq!(
            exec_script(App::Postgres, "psql -U postgres -q"),
            "sudo k3s kubectl exec -i -n default deploy/postgres -- sh -c 'psql -U postgres -q'"
        );
        for a in [App::Postgres, App::Mysql, App::Mongodb] {
            let t = tools(a).unwrap();
            assert!(!t.dump.contains('\'') && !t.restore.contains('\''));
        }
    }

    #[test]
    fn the_default_file_names_the_app_and_the_time() {
        assert_eq!(
            default_file(App::Postgres, "20261026-101500"),
            PathBuf::from("postgres-20261026-101500.sql")
        );
        assert_eq!(
            default_file(App::Mongodb, "x"),
            PathBuf::from("mongodb-x.archive")
        );
    }

    #[test]
    fn a_dump_file_is_private_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fluxctl-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.sql");
        write_private(&f, b"data").unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_private(&f, b"other").is_err());
        assert_eq!(std::fs::read(&f).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
