# benchit-telemetry

Workstation telemetry for benchmarking on Linux GPU boxes: a logger daemon that keeps a
rolling **30-day SQLite history**, an **HTTP JSON API**, a **CLI** for summaries, exports and
benchmark windows, and **panel feeds** for desktop widgets (conky, the bundled GTK panel).

> Status: **0.1.0-dev, pre-review.** APIs and the CLI may still change before 0.1.0.

## What it records

| Area | Source | Metrics |
|---|---|---|
| CPU | `/proc/stat`, cpufreq, `/proc/loadavg`, hwmon (`k10temp`, `coretemp`, vendor WMI) | utilisation, avg clock, load, Tctl/package temp, CCD or hottest-core temps |
| CPU power | RAPL powercap (`package-0`, `psys`) | package and platform watts (needs a readable `energy_uj`, see below) |
| Memory | `/proc/meminfo`, `/proc/vmstat` | used %, available, swap used, major faults/s, swap in/out per s |
| Disk | `/proc/diskstats`, `statvfs` | read/write MB/s and busy % of physical disks; `/` and `$HOME` usage |
| Network | `/proc/net/dev` | RX/TX MB/s of physical NICs |
| NVIDIA GPUs | `nvidia-smi --query-gpu` | temp, board power and limit, util, VRAM, SM/memory clocks, fan, P-state, clock-event (throttle) reasons |
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

Requirements: Linux, a C compiler (SQLite is bundled), `nvidia-smi` for GPU metrics (optional).
No root is needed. RAPL energy counters are root-only on many distributions; to let the
logger read CPU package power, an administrator can run (this exposes energy counters to all
local users, which has known side-channel implications):

```sh
echo 'z /sys/class/powercap/intel-rapl:0/energy_uj 0444 - - -' | sudo tee /etc/tmpfiles.d/benchit-rapl.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/benchit-rapl.conf
```

## Files

Data directory: `$BENCHIT_TELEMETRY_DIR`, else `$XDG_STATE_HOME/benchit-telemetry`, else
`~/.local/state/benchit-telemetry`.

| File | Written | Content |
|---|---|---|
| `metrics.db` | every `--interval` (10 s) | SQLite history (WAL), pruned hourly to `--retain-days` (30) |
| `current.json` | every `--tick` (2 s) | latest native snapshot (`benchit-telemetry current`) |
| `latest.json` | every tick | flat panel feed for desktop widgets (also `--panel-json PATH`) |
| `fast-until` | by `run` | while it holds a future unix time, every tick is stored |

## CLI

```text
benchit-telemetry daemon [--tick 2] [--interval 10] [--retain-days 30] [--listen 127.0.0.1:9184]
                         [--panel-json PATH]... [--read-only] [--no-store] [--no-gpu]
benchit-telemetry serve  [--listen 127.0.0.1:9184]     # API + live data, no storing
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

Times (`--from`, `--to`, API `from`/`to`) accept unix seconds, relative durations (`-15m`,
`2h`, `7d`), `now`, or local timestamps such as `2026-01-31 08:00`.

Benchmark a tool:

```sh
benchit-telemetry run llama-bench -- ./llama-bench -m model.gguf
# or bracket anything yourself
benchit-telemetry mark start my-test; ./my-test; benchit-telemetry mark end my-test
benchit-telemetry summary --mark my-test
```

## HTTP API

Started with `daemon --listen ADDR` (or `serve`). Binds to localhost by default; there is no
authentication, so do not expose it on untrusted networks.

| Method | Path | Description |
|---|---|---|
| GET | `/health` | status, version, last sample time, recording info |
| GET | `/metrics/current` | latest native snapshot |
| GET | `/metrics/panel` | latest flat panel JSON |
| GET | `/metrics/history?from=-1h&to=now&fields=a,b&step=60&limit=20000` | stored samples (bucket-averaged with `step`) |
| GET | `/metrics/gpus?from=-1h&idx=0&step=60` | stored per-GPU samples |
| GET | `/metrics/summary?from=-15m` or `?mark=NAME` | min/avg/p95/max per metric, energy (Wh), per-GPU stats |
| GET | `/metrics/fields` | queryable field names |
| GET | `/marks?limit=50` | recent benchmark marks |
| POST | `/marks` `{"name": "...", "cmd": "..."}` | start a mark (disabled with `--read-only`) |
| POST | `/marks/end` `{"name": "..."}` or `{"id": 1, "exit_code": 0}` | end a mark |

```sh
curl -s 'http://127.0.0.1:9184/metrics/history?from=-10m&fields=cpu_util_pct,gpu_total_w&step=30'
```

## Database schema (v1)

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
`samples_wide` — view: `local_time`, all `samples` columns, and `gpu0_*`/`gpu1_*` columns.
`meta` — `schema_version`, `retain_days`, `sample_interval_s`.

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
* **GTK panel**: `contrib/gtk-panel/benchit-panel.py` (+ `contrib/systemd/benchit-panel.service`)
  is a display-only widget that reads `latest.json` and seeds sparklines from `metrics.db`. Its
  window sizes itself to its content and stays inside the monitor work area. Right-click toggles
  compact mode, left-drag moves it, middle-click quits.

## Crates

* `benchit-telemetry-core` — library: `Collector`, `Snapshot` model, `Store`, panel renderers.
* `benchit-telemetry` — binary: daemon, API (`benchit_telemetry::api::router`), CLI.

## Development

```sh
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
Contributions are accepted under the same terms.
