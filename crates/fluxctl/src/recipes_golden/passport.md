# Cluster passport

Generated 2026-11-21T10:00:00Z by fluxctl 0.4.21. Server: http://192.168.1.20:7799

This document contains no passwords, tokens or keys.

## Summary

- Machines: 2 (1 online, 1 offline, 0 cordoned, 0 outdated)
- Machines asking to join: 1

## Machines and roles

| Name | Role | CPUs | Memory | VMs | Version | Status | Last seen |
|---|---|---|---|---|---|---|---|
| mini-1 | Mac (macOS VMs, Docker, k3s) | 10 | 32 GiB | 2 | 0.4.21 | online | 2026-11-21T09:59:50Z |
| box-1 | Linux (VMs) | 8 | 16 GiB | 0 | 0.4.20 | offline | 2026-11-20T10:00:00Z |

## Versions

- fluxctl on the machine that made this report: 0.4.21
- node agent 0.4.20: 1 machine(s)
- node agent 0.4.21: 1 machine(s)
- Different versions are running; upgrade the older machines.

## How to add a machine

On the new machine (Mac mini or Linux box), with fluxctl installed:
    fluxctl cluster pair
It finds this cluster by itself and shows a short code like ABCDE-FGHJK.
On the server, check that the code matches the machine's screen, then approve it:
    fluxctl cluster pending
    fluxctl cluster approve ABCDE-FGHJK
(or press Approve on the Studio page "Machines asking to join").
If the network blocks discovery: `fluxctl cluster pair --server http://192.168.1.20:7799`.
Pairing is on for this server.

## How to back up and restore

The server keeps everything in /var/lib/fluxvm-agent. Back it up while central is stopped or idle:
    tar czf fluxvm-central-backup.tgz -C /var/lib/fluxvm-agent . && chmod 600 fluxvm-central-backup.tgz
The archive holds the keys that make machine credentials: keep it private and off shared drives.
To restore: stop central, unpack the archive into the same folder, start central again.
Machines keep their own credential; they reconnect by themselves. A machine that lost its own state pairs again.
Apps from recipes: see each recipe's backup line (`fluxctl recipe show NAME`).

## How to undo

Stop sending work to a machine:  fluxctl fleet cordon NAME
Take a machine out:              fluxctl fleet deregister NAME   (its credential stops working)
A machine was approved by mistake: deregister it, then check the Activity page in Studio.
Stop accepting new machines:     restart central without --pairing.
Go back to the previous FluxVM version on a Linux machine: fluxvm-bootstrap rollback
Remove an app from a recipe:     fluxctl recipe uninstall NAME

## Where the logs are

Mac, node agent:  ~/Library/Logs/fluxvm-agent.log
Linux, services:  journalctl -u fluxvm-agent -u fluxvm --since today
Server (central): journalctl -u fluxvm-agent, or the terminal it was started in
Activity and audit trail: Studio, page Activity.

## Apps installed from recipes

- registry

## Contacts and notes

Call Ana on 555-0101 if the power fails.
