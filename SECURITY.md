# Security policy

## What the HTTP API exposes

`benchit-telemetry daemon --listen` / `serve` start an **unauthenticated** JSON API. Anything that
can reach it can read the hostname, CPU/GPU models, filesystem mount paths (which reveal `$HOME`),
the name of the largest process, and the command lines of benchmark marks, and, unless the API
runs with `--read-only`, create and close marks.

Built-in defences (see `crates/benchit-telemetry/src/api.rs`):

* it listens on loopback by default;
* requests whose `Host` header is not `localhost`, `127.0.0.1`, `[::1]` or an `--allow-host` get
  421, which stops DNS-rebinding attacks from web pages;
* a present `Origin` header must also be local (403 otherwise);
* no CORS headers are sent and POSTs require `Content-Type: application/json`;
* result sizes, concurrent queries, request time and body size are bounded.

It does **not** separate users on the same machine: any local user can connect to a loopback
port. Use `--read-only`, omit `--listen` (the panel and CLI read the data directory directly), or
firewall the port on shared machines. Do not bind it to a non-loopback address on untrusted networks.

The data directory (`metrics.db`, `current.json`, `latest.json`) is created `0700`/`0600`.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub's *Security → Report a
vulnerability* on this repository (private vulnerability reporting), rather than a public issue.
We aim to acknowledge within a week.
