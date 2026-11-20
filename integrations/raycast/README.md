# Raycast script commands for `fluxctl kube`

One keystroke for the things you do all day: start the cluster, save a checkpoint, undo the last `kube try`, sleep and
wake it, see `kube ps`, run `kube doctor`, and open a service on your phone with `kube share`.

1. In Raycast, open Settings, Extensions, Script Commands, **Add Script Directory**, and choose this folder (copy it
   somewhere permanent first if you installed fluxctl with Homebrew).
2. Type "Kubernetes" in Raycast.

Each script finds `fluxctl` on the PATH Raycast gives it plus `/opt/homebrew/bin` and `/usr/local/bin`; set
`FLUXCTL=/path/to/fluxctl` to use another one. They are plain `bash`, so the same files work from Alfred's "Run Script"
or Apple Shortcuts' **Run Shell Script** action.

`scripts/test-raycast-scripts.sh` checks that every script has the headers Raycast needs and parses.
