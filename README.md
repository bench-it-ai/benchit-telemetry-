# benchit-telemetry

Workstation telemetry for benchmarking on Linux GPU boxes: a logger daemon that keeps a
rolling **30-day SQLite history**, an **HTTP JSON API**, a **CLI** for summaries, exports and
benchmark windows, and **panel feeds** for desktop widgets (conky, the native desktop panel).

> Status: **0.1.0-dev.** An independent review (blockers and majors fixed, see `CHANGELOG.md`) has
> been done; APIs and the CLI may still change before 0.1.0.

## What it records

| Area | Source | Metrics |
|---|---|---|
| CPU | `/proc/stat`, cpufreq, `/proc/loadavg`, hwmon (`k10temp`, `coretemp`, vendor WMI) | utilisation, avg clock, load, Tctl/package temp, CCD or hottest-core temps |
| CPU power | RAPL powercap (`package-0`, `psys`) | package and platform watts (needs a readable `energy_uj`, see below) |
| Memory | `/proc/meminfo`, `/proc/vmstat` | used %, available, swap used, major faults/s, swap in/out per s |
| Disk | `/proc/diskstats`, `statvfs` | read/write MB/s and busy % of physical disks; `/` and `$HOME` usage |
| Network | `/proc/net/dev` | RX/TX MB/s of physical NICs |
| NVIDIA GPUs | NVML (default feature `nvml`), falling back to `nvidia-smi --query-gpu` | temp, board power and limit, util, VRAM, SM/memory clocks, fan, P-state, clock-event (throttle) reasons |
| Other | hwmon, power_supply, `/proc` | fan RPMs, battery %, battery W, AC state, largest process, uptime, warning flags |

## Install

```sh
cargo install --git https://github.com/benchit/benchit-telemetry benchit-telemetry
# or from a checkout
cargo build --release && install -Dm755 target/release/benchit-telemetry ~/.local/bin/benchit-telemetry

install -Dm644 contrib/systemd/benchit-telemetry.service ~/.config/systemd/user/benchit-telemetry.service
systemctl --user daemon-reload
systemctl --user enable --now benchit-telemetry
loginctl enable-linger "$USER"     # optional: keep logging without an active login
```

Requirements: Linux, a C compiler (SQLite is bundled), an NVIDIA driver for GPU metrics (optional:
NVML, `libnvidia-ml.so.1`, is loaded at run time, and `nvidia-smi` is the fallback; build with
`--no-default-features` to drop the NVML crate). Minimum Rust is 1.89 for the library, daemon
and CLI; the native panel needs 1.95.

**Hybrid laptops:** a dGPU whose PCI `power/runtime_status` is `suspended` is never queried, so
benchmarking power is not distorted by the logger waking it; it is reported with `pstate = "D3"`
and no readings.
No root is needed. RAPL energy counters are root-only on many distributions; to let the
logger read CPU package power, an administrator can run (this exposes energy counters to all
local users, which has known side-channel implications):

```sh
# one line per zone: package-0 is usually intel-rapl:0, psys intel-rapl:1; multi-socket machines
# have intel-rapl:N for each socket (check /sys/class/powercap/*/name)
echo 'z /sys/class/powercap/intel-rapl:0/energy_uj 0444 - - -
z /sys/class/powercap/intel-rapl:1/energy_uj 0444 - - -' | sudo tee /etc/tmpfiles.d/benchit-rapl.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/benchit-rapl.conf
```

## Files

Data directory (created `0700`; files `0600`), first match wins:

1. `$BENCHIT_TELEMETRY_DIR` / `--dir`;
2. `~/telemetry`, **only if it already contains `metrics.db`** and the standard directory does not
   (machines set up with the first layout keep their history without changes);
3. `$XDG_STATE_HOME/benchit-telemetry`, i.e. `~/.local/state/benchit-telemetry` (new installs).

A daemon holds an `flock` on `daemon.lock` there, so two daemons cannot share a directory.

| File | Written | Content |
|---|---|---|
| `metrics.db` | every `--interval` (10 s) | SQLite history (WAL), pruned hourly to `--retain-days` (30) |
| `current.json` | every `--tick` (2 s) | latest native snapshot (`benchit-telemetry current`) |
| `latest.json` | every tick | flat panel feed for desktop widgets (also `--panel-json PATH`) |
| `fast-until.d/<pid>` | by `run` | 30-second leases refreshed while a `run` is alive; while any live one is unexpired, every tick is stored (the old single `fast-until` file is still honoured) |
| `daemon.lock` | daemon | `flock` that stops a second daemon |

Storage rows are **interval averages**: CPU %, package/platform watts, fault rates and disk and
network throughput are averaged over the whole `--interval`, and GPU power and utilisation over
the ticks within it (not just the last tick). In fast mode (a `run` is active) rows are per tick.

## CLI

```text
benchit-telemetry daemon [--tick 2] [--interval 10] [--retain-days 30] [--listen 127.0.0.1:9184]
                         [--panel-json PATH]... [--read-only] [--allow-host NAME]...
                         [--no-store] [--no-gpu] [--no-top-process]
benchit-telemetry serve  [--listen 127.0.0.1:9184] [--read-only] [--allow-host NAME]...  # API + live data, no storing
benchit-telemetry current [--pretty]                     # snapshot JSON
benchit-telemetry panel  [--format text|json|flat] [--width 44]
benchit-telemetry status
benchit-telemetry summary [--since 30m | --from T --to T | --mark NAME] [--json]
benchit-telemetry history [--since 1h] [--fields cpu_util_pct,gpu_total_w] [--step 60] [--json]
benchit-telemetry gpus    [--since 1h] [--idx 0] [--step 60] [--json]
benchit-telemetry export  [--since 7d] [--out week.csv]
benchit-telemetry mark start|end NAME ;  benchit-telemetry marks
benchit-telemetry run NAME -- COMMAND [ARGS...]           # fast sampling + summary of the run
benchit-telemetry fields
```

Times (`--from`, `--to`, API `from`/`to`) accept:

* relative durations: `-15m`, `-90s`, `-7d`, `-90`; a unit without the minus (`2h`) also means
  "2 h ago";
* `now`;
* unix seconds, which must be at least `1e8` — a bare small number such as `90` is rejected as
  ambiguous (write `-90s`), as are `inf`, `nan` and absurd values;
* local timestamps `2026-01-31 08:00[:SS]` (or with `T`); a trailing `Z` or `±HH:MM` makes the
  time absolute. A local time that does not exist (DST gap) is shifted forward by SQLite.

`--since` conflicts with `--from`, and `--mark` with all of them. The default window is 1 h for the
CLI and the API.

`run` forwards SIGINT/SIGTERM/SIGHUP sent to it to the command and survives them, so the mark is
always closed (with `exit_code` = the command's, or 128+signal, and a `note`) and its lease
removed. Exit codes: the command's, 128+N if it died from signal N, 127 not found, 126 not
executable. If no daemon is feeding the data directory it warns that nothing will be recorded
and does not wait.

Benchmark a tool:

```sh
benchit-telemetry run llama-bench -- ./llama-bench -m model.gguf
# or bracket anything yourself
benchit-telemetry mark start my-test; ./my-test; benchit-telemetry mark end my-test
benchit-telemetry summary --mark my-test
```

## HTTP API

Started with `daemon --listen ADDR` (`daemon` starts no API without `--listen`; `serve` always
does, on `127.0.0.1:9184` unless told otherwise).

**The API is unauthenticated and local-only; read [SECURITY.md](SECURITY.md).** It can show the
hostname, hardware, `$HOME` path, the largest process name and benchmark command lines. Requests
must carry a `Host` of `localhost`, `127.0.0.1`, `[::1]` or an `--allow-host` name (otherwise
421, which defeats DNS rebinding), a present `Origin` must be local (otherwise 403), no CORS
headers are sent, and POSTs need `Content-Type: application/json`.

Stable paths live under `/api/v1`; `/metrics/*` and `/marks*` remain as deprecated aliases
(`/metrics` is reserved for a future Prometheus exporter).

| Method | Path (under `/api/v1`) | Description |
|---|---|---|
| GET | `/health` (also `/health`) | `200 ok`; `503` while `starting` or `stale` (newest sample older than 3 ticks, e.g. the collector died) |
| GET | `/current` | latest native snapshot |
| GET | `/panel` | latest flat panel JSON |
| GET | `/history?from=-1h&to=now&fields=a,b&step=60&limit=10000` | stored samples; `limit` is capped at 50 000 and a window with more rows is bucketed automatically (`auto_step`, `truncated` in the reply) |
| GET | `/gpus?from=-1h&idx=0&step=60` | stored per-GPU samples |
| GET | `/summary?from=-15m` or `?mark=NAME` | min/avg/p95/max per metric, `energy_est_wh` (+ `energy_sources`, `coverage_s`), per-GPU stats |
| GET | `/fields` | queryable field names |
| GET | `/marks?limit=50` | `{count, marks}` (the legacy `/marks` returns a bare array) |
| POST | `/marks` `{"name": "...", "cmd": "..."}` | start a mark (403 with `--read-only`) |
| POST | `/marks/end` `{"name": "..."}` or `{"id": 1, "exit_code": 0}` | end a mark: 404 unknown id/name, 409 already closed |

Errors are JSON `{"error": ...}` with real status codes: 400 bad input, 403, 404, 409, 421, 503
(no database, too many concurrent queries — at most 4 run at once), 504 (10 s budget); internal
details and file paths are logged, never returned. Bodies over 16 KiB are rejected.

```sh
curl -s 'http://127.0.0.1:9184/api/v1/history?from=-10m&fields=cpu_util_pct,gpu_total_w&step=30'
```

**Energy is an estimate.** `energy_est_wh` integrates `est_total_w` (GPU board power plus CPU
package power *when RAPL is readable* — without it, GPUs only; `energy_sources` says which) with the
trapezoid rule, and skips any gap longer than three sample intervals (daemon stopped, machine
suspended) instead of interpolating across it; `coverage_s` is the time actually integrated.
`platform_energy_est_wh` does the same for whole-platform `platform_w` where RAPL `psys` exists.

## Database schema (v2)

`samples` — one row per stored sample, primary key `(host, ts)`:

| column | unit | meaning |
|---|---|---|
| `ts` | unix s | sample time (`datetime(ts,'unixepoch','localtime')` for local time) |
| `host` | | short hostname |
| `cpu_temp_c`, `cpu_temp2_c`, `cpu_temp3_c` | °C | Tctl/package; CCD1 or hottest core; CCD2 |
| `cpu_util_pct`, `cpu_mhz`, `load1` | %, MHz | |
| `cpu_pkg_w`, `platform_w` | W | RAPL package / psys (NULL when unreadable) |
| `gpu_total_w`, `est_total_w` | W | sum of GPU board power; GPUs + CPU package |
| `mem_total_mb`, `mem_avail_mb`, `mem_used_pct`, `swap_used_mb` | MiB, % | |
| `majflt_s`, `swpin_s`, `swpout_s` | 1/s | |
| `root_used_gb`, `root_total_gb`, `root_used_pct`, `home_*` | GB, % | `home_*` NULL when `$HOME` is on `/` |
| `disk_read_mbs`, `disk_write_mbs`, `disk_busy_pct` | MB/s, % | physical disks |
| `net_rx_mbs`, `net_tx_mbs` | MB/s | physical NICs |
| `bat_pct`, `bat_w`, `ac_online` | %, W, 0/1 | laptops |
| `fans_json`, `top_proc`, `flags`, `uptime_s` | | `[{"name","rpm"}]`, `"<MiB>:<comm>"`, space-separated flags |

`gpu_samples` — primary key `(host, ts, idx)`: `name, temp_c, power_w, power_limit_w,
util_pct, vram_used_mb, vram_total_mb, sm_mhz, mem_mhz, fan_pct, pstate, clk_event_reasons`.

`marks` — `id, host, name, t_start, t_end, cmd, exit_code, note`.
`samples_wide` — view: `local_time`, all `samples` columns, and `gpu0_*` … `gpu3_*` columns.
`meta` — `schema_version`, `retain_days`, `sample_interval_s`.

The schema version is `PRAGMA user_version` (v1 = first release, v2 adds the `marks(name, id)`
index and the four-GPU view). Opening a database with an older version migrates it in one
transaction (the first writer does it; read-only commands tell you to start the daemon); a
database written by a **newer** version is refused and left untouched. Marks are kept when
samples are pruned. Rows hold interval averages, with one-second timestamps.

Example: GPU behaviour during a mark

```sql
SELECT g.idx, round(avg(g.power_w),1) AS avg_w, max(g.temp_c) AS max_c,
       round(avg(g.util_pct),1) AS avg_util, round(avg(g.sm_mhz)) AS avg_sm_mhz
FROM marks m JOIN gpu_samples g ON g.ts BETWEEN m.t_start AND m.t_end
WHERE m.name = 'llama-bench' GROUP BY g.idx;
```

## Desktop panels

* **conky**: `contrib/conky/benchit.conkyrc` runs `benchit-telemetry panel --width 44`; text
  lines are ellipsised to the width, never clipped.
* **Native panel** (`benchit-panel`, crate `benchit-telemetry-panel`, egui): an always-on-top,
  display-only window fed by the daemon's `current.json` (read through `benchit-telemetry-core`),
  with sparklines seeded from `metrics.db`. It sizes itself to its content, stays inside the
  monitor work area, and shows every GPU, storage/network and the recording status. Right-click
  toggles compact mode (remembered in `~/.config/benchit-panel.json`), left-drag moves it,
  middle-click quits. `benchit-panel --snapshot out.png [--compact]` renders one frame to a PNG.
  It stays above other windows and on every workspace (EWMH always-on-top and sticky, applied after mapping — GNOME ignores the pre-map hint). It runs on X11 (XWayland under GNOME, because Wayland clients cannot position windows) and
  uses Mesa software GL so it never touches a benchmark GPU (`BENCHIT_PANEL_GPU=1` to opt out).
  Needs only runtime libraries that desktops already have (libX11, libGL/libEGL, libxkbcommon);
  fonts Inter and JetBrains Mono are used if installed. Unit: `contrib/systemd/benchit-panel.service`.

## Crates

* `benchit-telemetry-core` — library: `Collector`, `Snapshot` model, `Store`, panel renderers.
* `benchit-telemetry` — binary: daemon, API (`benchit_telemetry::api::router`), CLI.
* `benchit-telemetry-panel` — binary `benchit-panel`: native desktop panel.

## Development

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
Contributions are accepted under the same terms.
