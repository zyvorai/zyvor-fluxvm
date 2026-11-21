# The cluster passport

> **Status.** The report builder, the three formats (golden files), the secret scrubbing and the read-only page's request handling are unit-tested.
> **Not run** against a large or live fleet; the machine list comes from `GET /fleet/nodes` as documented.

One document you can give to someone else (a colleague, a landlord, yourself in a year) that says what this cluster is and how to look after it.

```bash
fluxctl cluster report                       # Markdown on the screen
fluxctl cluster report --format html --out passport.html
fluxctl cluster report --format json         # for scripts
fluxctl cluster report --serve               # a read-only page at http://127.0.0.1:7801/
```

Run it on the server (it reads the operator token the same way `cluster pending` does) or pass `--server` and `--token-file`. If the server cannot be
asked, the report is still produced, with a note saying so.

## What is in it

1. **Summary**: machines, how many are online, offline, cordoned or outdated, machines asking to join.
2. **Machines and roles**: name, role (Mac or Linux), CPUs, memory, VMs, agent version, status, last seen.
3. **Versions**: fluxctl and each node agent version, with a warning when they differ.
4. **How to add a machine**: `fluxctl cluster pair`, the approve step, the `--server` fallback, and whether pairing is on ([cluster-pairing.md](cluster-pairing.md)).
5. **How to back up and restore** the server's state directory.
6. **How to undo**: cordon, deregister, stop pairing, roll back a version, uninstall a recipe app.
7. **Where the logs are** on a Mac, on Linux and on the server.
8. **Apps installed from recipes** ([recipes.md](recipes.md)).
9. **Contacts and notes**: the contents of `~/.config/fluxvm/passport-notes.md` (or `--notes-file FILE`). Who to call, where the spare mini is.

## Secrets are never in it

Two layers. The report is built from a fixed structure that has no field for a token, credential, password or key (a test checks the field names).
Then everything written, in every format, passes a scrubber that replaces anything shaped like a secret: `fvnc_`/`fvjt_` credentials, GitHub, Slack
and AWS style tokens, JWTs, long hex strings, `password=`/`token:`/`Bearer ...` values and `user:password@` in URLs. A test pastes a dozen such values into the notes,
a machine name, the server address and a note and checks that none appears in Markdown, HTML or JSON. The scrubber errs toward removing too much:
a very long random-looking word in your notes may be replaced by `[redacted]`. It is a safety net, not permission to paste secrets into the notes file.

The backup advice names the files in the state directory but never reads them; note that the archive itself contains the enrollment secret and must be kept private.

## `--serve`

Serves the same report on 127.0.0.1 only, read-only: `GET` only, the `Host` header must be `127.0.0.1:PORT` or `localhost:PORT` (so a web page elsewhere cannot read it
through DNS rebinding), strict CSP, no cache, `/` (HTML), `/report.md` and `/report.json`. Each request looks at the cluster again.
There is no authentication because it listens on loopback; do not forward the port.
