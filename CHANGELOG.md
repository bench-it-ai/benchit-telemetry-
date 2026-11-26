# Changelog

## 0.1.0-dev (unreleased, pre-review)
- Collector: CPU util/clock/temps (k10temp, coretemp, vendor WMI), RAPL package/psys power,
  memory/swap/page-fault rates, disk and network throughput, filesystem usage, fans, battery,
  top process, NVIDIA GPUs via `nvidia-smi`.
- SQLite store (schema v1) with 30-day retention, incremental vacuum, benchmark marks.
- `daemon`, `serve`, `current`, `panel`, `status`, `history`, `gpus`, `summary`, `export`,
  `mark`, `marks`, `run`, `fields` subcommands.
- HTTP JSON API (axum).
- contrib: systemd user units, conky config, GTK desktop panel.
