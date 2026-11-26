# Changelog

## 0.1.0-dev, panel visibility
- The panel now sends `_NET_WM_STATE_ABOVE` and `_NET_WM_STATE_STICKY` as EWMH ClientMessages
  after it is mapped. `with_always_on_top()` alone is ignored by GNOME/XWayland, so the window
  was on screen but hidden behind maximized apps (and only on the workspace where it started).

## 0.1.0-dev, after the independent review
Fixes for every blocker and major finding (and most minors) of the review of the first commit:
- `run` survives SIGINT/SIGTERM/SIGHUP (forwarded to the command), always closes its mark and
  removes its lease, uses short per-process leases (`fast-until.d/<pid>`, 30 s) instead of a
  7-day file, exits 128+signal (126/127 for start failures), and warns when no daemon is running.
- API: `Host`/`Origin` allow-list (DNS rebinding), limits capped at 50 000 with automatic
  bucketing, 4 concurrent queries, 10 s request budget, 16 KiB bodies, typed status codes (404/409
  for marks, 503 for a missing database), no paths in errors, `/api/v1` paths, `/health` goes 503
  when stale or starting.
- Storage: rows hold averages over the whole interval, GPU power/util are averaged over the ticks;
  energy no longer spans gaps, is labelled `energy_est_wh` with `energy_sources` and `coverage_s`.
- GPU: NVML (feature `nvml`, default) with an `nvidia-smi` fallback, a field ladder for pre-R535
  drivers, exit-status check with exponential backoff, collection on its own thread with a
  timeout, runtime-suspended hybrid-laptop dGPUs are never woken.
- Time parsing: bare small numbers and non-finite values are rejected (no more Julian days);
  `Z`/offset timestamps tested. Schema version in `user_version`, migration and refusal of newer
  databases. Version is `0.1.0-dev` everywhere; one documented data directory with automatic
  support for the old `~/telemetry`; a dead collector thread stops the daemon and turns `/health`
  to 503; redundant `homepage` manifest keys removed.
- Also: monotonic scheduling (clock steps), tick validation, 0700/0600 files, daemon `flock`,
  private per-process temp files, `serve` cleans up, RAPL wrap without a known range skips the
  sample, 32-bit `io_ticks` wrap, battery/AC aggregation, `df`-style used %, Intel `temp2`,
  periodic sensor re-discovery, top-process scan every 10 s (`--no-top-process`), CSV formula
  injection guard, marks kept past retention, `mark_end` only closes open marks, indexed mark
  lookup, flag names `WARN_MEM_LOW`/`WARN_BATTERY_LOW`, conflicting CLI window flags, CI hardening
  (pinned actions, `cargo doc`, `cargo deny`, MSRV tests), SECURITY.md/CONTRIBUTING.md.

## 0.1.0-dev (unreleased, pre-review)
- Collector: CPU util/clock/temps (k10temp, coretemp, vendor WMI), RAPL package/psys power,
  memory/swap/page-fault rates, disk and network throughput, filesystem usage, fans, battery,
  top process, NVIDIA GPUs via `nvidia-smi`.
- SQLite store (schema v1) with 30-day retention, incremental vacuum, benchmark marks.
- `daemon`, `serve`, `current`, `panel`, `status`, `history`, `gpus`, `summary`, `export`,
  `mark`, `marks`, `run`, `fields` subcommands.
- HTTP JSON API (axum).
- contrib: systemd user units, conky config, native egui desktop panel (benchit-panel).
