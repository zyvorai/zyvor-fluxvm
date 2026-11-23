# Apps on a cluster: `fluxctl kube run`

Run a container image on a FluxVM k3s cluster the way you run one with Docker, and look after it with a few short commands. Everything
works with no set-up: with one cluster on the machine it is used automatically. With several, add `--cluster NAME`.

```sh
fluxctl kube run nginx                    # Deployment + Service, waits until ready, prints http://127.0.0.1:8080/
fluxctl kube run ghcr.io/you/api:1.0 --port 3000 --env MODE=dev --replicas 2
fluxctl kube apps                         # the services `kube add` can run, then your apps with their state
fluxctl kube logs web -f                  # every pod of the app in one stream
fluxctl kube shell web                    # a shell in it (or `kube shell web -- ls /app`)
fluxctl kube forward web 8080             # a port on this Mac that stays (needs `kube supervise`)
fluxctl kube scale web 3                  # 0 stops it without deleting it
fluxctl kube rollout undo web             # back to the previous version (`rollout restart` restarts it)
fluxctl kube expose web                   # http://web.localhost:PORT through the Gateway (see gateway.md)
fluxctl kube rm web                       # delete it (and its gateway route)
```

## What `run` does

1. Picks the cluster: `--cluster NAME` or `--kube-context`, otherwise the only `~/.kube/fluxvm-*.yaml`, otherwise kubectl's current context.
2. Checks the name is free. Anything with that name that `kube run` did not make is never touched (`APP_NAME_TAKEN`); running the
   same command again updates your own app.
3. Applies a Deployment and a Service named after the app (default: the image's name, `--name` to choose) with the label
   `app.kubernetes.io/managed-by=fluxctl-app`. `list`, `scale`, `rm`, `rollout` and Studio only ever act on objects with that label.
4. Waits until the app is ready (`--wait SECS`, default 180). If a pod shows a pull error or a crash on two looks in a row it stops waiting
   and says what is wrong and what to run next. The app is left in the cluster so you can read its logs; `kube rm` removes it.
5. Prints the URL: a free local port (from 8080, a real bind; `--local-port N` asks for one) is forwarded to the Service until Ctrl-C.
   The app keeps running. With `--detach`, or when there is no terminal (scripts, CI), nothing is forwarded and the command ends after
   creating the app.

The port defaults to the well-known one for common images (grafana 3000, redis 6379, ...) and to 80 otherwise; `--port N` says what the app listens on.

## When something is wrong

Every failure says what happened and what to do, and ends with a code that `fluxctl explain CODE` expands:

| Code | You see |
| --- | --- |
| `APP_IMAGE_PULL` | The cluster cannot download the image: check the name, or `fluxctl kube load IMAGE` for one built here |
| `APP_CRASHING` | It starts and stops: read `fluxctl kube logs APP --previous` |
| `APP_OUT_OF_MEMORY` | Give the cluster more memory (`kube up --memory-mib 8192`) or run fewer copies |
| `APP_NOT_READY` | Still not ready after the wait, or no machine has room |
| `APP_NO_CLUSTER` | No cluster or no kubectl: `fluxctl kube up` |
| `APP_CLUSTER_AMBIGUOUS` | Several clusters: add `--cluster NAME` |
| `APP_PORT_TAKEN` | The local port is in use: leave out `--local-port` and one is picked |
| `APP_NAME_TAKEN` | Something not made by `kube run` has that name |
| `APP_NOT_FOUND` | No such app in that namespace: `fluxctl kube apps`, `-n NAMESPACE` |

## In Studio

The **Apps** page (Platform, in the side bar) is the same thing: a Run form (image, name, port, copies), the list with each app's state
in one sentence (a problem shows the next command and its explain code), Logs and Shell windows (the pod windows of the Kubernetes
page), fewer or more copies, Remove (asks twice) and **Copy as fluxctl** for the command that does the same.

The page uses `GET /v1/kube/{cluster}/apps` (`schema: 1`; not reaching the cluster is an `error` field with the next command, not an HTTP
failure), `POST /v1/kube/{cluster}/apps` (admin; `{"image", "name"?, "port"?, "replicas"?, "namespace"?, "env"?}`), `POST
/v1/kube/{cluster}/apps/{ns}/{app}/delete` (admin) and the existing workload scale route. These talk to the cluster's API server directly,
like the rest of the Kubernetes page.

## Not covered

No private-registry credentials (`kube load` an image instead), no volumes or resource limits, and no Ingress other than the Gateway
(`kube expose`). `kube logs`, `kube forward` and `kube apps` are the older commands and take `--name` for the cluster; the new ones take
`--cluster`.
