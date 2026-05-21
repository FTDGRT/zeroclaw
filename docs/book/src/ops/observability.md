# Logs & observability

Every event ZeroClaw emits flows through one crate: `zeroclaw-log`. The crate
owns the on-disk JSONL schema, the in-process broadcast stream the dashboard
reads, the bridge to the typed `Observer` (Prometheus / OTel), and the
macros (`record!`, `scope!`, `spawn!`) that subsystems call.

This page covers what an operator needs: configuration, where the log lives,
the shape of the events, and how to query them.

## Config (`[observability]`)

```toml
[observability]
# Storage policy for the JSONL log.
# "none"    — in-process broadcast only (no disk writes).
# "rolling" — append + trim once `log_persistence_max_entries` is exceeded.
# "full"    — append forever, operator manages rotation.
log_persistence = "rolling"

# Workspace-relative path (or absolute).
log_persistence_path = "state/runtime-trace.jsonl"

# Cap for "rolling".
log_persistence_max_entries = 200

# Tool input/output capture policy.
# "off"      — only tool name + outcome + duration; no I/O bodies.
# "redacted" — bodies are leak-scanned and truncated at `log_tool_io_truncate_bytes`.
# "full"     — bodies are leak-scanned; no truncation.
log_tool_io = "redacted"
log_tool_io_truncate_bytes = 8192

# Tool names whose I/O is never persisted beyond name + outcome + duration,
# regardless of `log_tool_io`. For tools whose I/O is intrinsically sensitive.
log_tool_io_denylist = []

# OTel / Prometheus backend (independent of the JSONL log).
backend = "none"            # "none" | "log" | "verbose" | "prometheus" | "otel"
otel_endpoint = "http://localhost:4318"
otel_service_name = "zeroclaw"
# otel_headers = { Authorization = "Bearer …" }
```

Defaults: `log_persistence = "rolling"`, `log_persistence_max_entries = 200`,
`log_tool_io = "redacted"`, `log_tool_io_truncate_bytes = 8192`. A fresh
install produces a 200-event rolling JSONL at
`~/.zeroclaw/state/runtime-trace.jsonl`, and the dashboard's Logs page
works without further configuration.

`log_persistence = "none"` disables persistence entirely. The broadcast
stream (dashboard SSE) and the typed `Observer` bridge still receive
events; only the JSONL writer is gated.

## On-disk format

JSONL: one event per line, UTF-8, `0o600` permissions on Unix. Every
line is `sync_data`'d after write — the line is durable before the
emitting code returns.

Line shape mirrors `zeroclaw_log::event::LogEvent`. Top-level keys:

| Key | Type | Notes |
| --- | --- | --- |
| `id` | UUID v4 string | Persistent event id. |
| `@timestamp` | RFC 3339 + ms, UTC | Lexicographic-sortable; the reader sorts on this. |
| `severity_number` | u8 | OTel: 1 TRACE, 5 DEBUG, 9 INFO, 13 WARN, 17 ERROR. |
| `severity_text` | string | Bucket label for `severity_number`. |
| `event.category` | string | `agent`, `channel`, `cron`, `memory`, `tool`, `provider`, `session`, `system`, or `internal`. |
| `event.action` | string | Stable identifier (`llm_request`, `channel_message_inbound`, …). |
| `event.outcome` | string \| omitted | `success`, `failure`, `unknown` (omitted when `unknown`). |
| `service.name` | string | Constant `"zeroclaw"`. |
| `service.version` | string | Crate version of the running daemon. |
| `trace_id` | hex string \| omitted | Per-turn correlation. One agent turn = one trace_id. |
| `span_id` | hex string \| omitted | Sub-span within a turn. |
| `zeroclaw.*` | flat string map | Alias-bound attribution (see below). |
| `message` | string \| omitted | Human-readable line body. |
| `attributes` | object \| omitted | Free-form per-action payload. |
| `schema_version` | u8 | Currently `2`. v1 rows migrate in-place on startup. |

### `zeroclaw.*` attribution

The Rust source of truth is `ATTRIBUTION_FIELDS` + `COMPOSITE_PREFIXES`
in `crates/zeroclaw-log/src/event.rs`. The `/api/logs` response carries
the canonical list as `attribution_keys`; fetch it instead of
hard-coding.

Plain fields (`ATTRIBUTION_FIELDS`) carry a single string each.
Composite prefixes get three keys: `<prefix>`, `<prefix>_type`,
`<prefix>_alias` (e.g. `channel = "discord.glados"`,
`channel_type = "discord"`, `channel_alias = "glados"`). Filters can
match either coarse or precise.

When a tracing call sets a composite-prefix field to a bare type (no
`.`), only the `_type` slot is populated — that way a
`tracing::*!(model_provider = name, …)` call inside a span that
already carries the full `<type>.<alias>` composite doesn't clobber it
on the leaf→root merge.

## Querying

The dashboard's Logs page is the primary surface. Underneath:

```
GET /api/logs
```

Top-level filters (query params): `since_ts`, `until_ts`, `until_id`,
`action`, `category`, `outcome`, `severity_min`, `trace_id`, `q`
(substring across `message` + `attributes`), `hide_internal` (drops
`event.category = "internal"`), `limit`.

Every other `?<key>=<value>` is treated as a per-attribution equality
filter — the gateway validates the key against `is_attribution_field`
and rejects unknowns with `400`. The response includes
`attribution_keys: string[]`, so callers don't have to guess.

Examples:

```bash
# All WARN+ events since the daemon started.
curl "$ZEROCLAW_GATEWAY/api/logs?severity_min=13"

# A specific agent's events:
curl "$ZEROCLAW_GATEWAY/api/logs?agent_alias=glados"

# Discord traffic for one bot:
curl "$ZEROCLAW_GATEWAY/api/logs?channel=discord.glados"

# A single agent turn:
curl "$ZEROCLAW_GATEWAY/api/logs?trace_id=<value-from-a-prior-event>"
```

Pagination is reverse-cursor. The response includes
`next_cursor: [timestamp, id] | null`; pass these back as `until_ts` +
`until_id` to load older. `at_end: true` means the reader scanned the
whole file for the current filter.

The `/api/status` response includes `daemon_started_at: string` (RFC
3339), so a dashboard can default to "since daemon start" without an
extra round-trip.

## External log viewers

The JSONL schema is an OTel-logs + ECS hybrid: `@timestamp`,
`severity_number` + `severity_text`, `event.{category,action,outcome}`,
`service.{name,version}`, `attributes`, plus the `zeroclaw.*` vendor
namespace. Most log viewers ingest it with little or no transform.
Replace `<install>` with the absolute path to your install dir in the
examples below (typically `~/.zeroclaw` expanded).

### Grafana Loki

Promtail labels lift `agent_alias`, `channel`, and `severity_text` so
they're filterable in Grafana:

```yaml
scrape_configs:
  - job_name: zeroclaw
    static_configs:
      - targets: [localhost]
        labels:
          job: zeroclaw
          __path__: <install>/data/state/runtime-trace.jsonl
    pipeline_stages:
      - json:
          expressions:
            agent: zeroclaw.agent_alias
            channel: zeroclaw.channel
            level: severity_text
      - labels:
          agent:
          channel:
          level:
      - timestamp:
          source: '@timestamp'
          format: RFC3339
```

### OpenTelemetry Collector

The `filelog` receiver maps the schema directly. Export to any OTel
sink afterward (Tempo, Honeycomb, Datadog, etc.):

```yaml
receivers:
  filelog/zeroclaw:
    include: [<install>/data/state/runtime-trace.jsonl]
    operators:
      - type: json_parser
        timestamp:
          parse_from: attributes["@timestamp"]
          layout: '%Y-%m-%dT%H:%M:%S.%LZ'
        severity:
          parse_from: attributes.severity_number
```

### Kibana / Elastic

Ingest works as-is. Strict ECS pipelines expect `log.level` in place
of `severity_text`. A Filebeat ingest pipeline that renames
`severity_text` to `log.level` (and `severity_number` to
`log.syslog.severity.code`) covers the gap. `@timestamp` and
`event.{category,action,outcome}` are already in canonical positions.

### Vector / Fluent Bit

Both tail JSONL with a JSON parser stage; no schema transforms needed
before shipping to any backend.

## File trace backend (upcoming)

> **Not yet wired into the runtime.** The `zeroclaw-file-rotation` crate is available as a workspace dependency, but the `backend = "file"` observer path, `file_path` config field, and `[observability.file_rotation]` schema are not yet connected to `zeroclaw-config` or `zeroclaw-runtime`. The configuration and event format below reflect the crate's public API and will be enabled in a follow-up PR that adds the file observer backend and wires the `RuntimeTraceLogger` through `RotatingFileWriter`.

The `file` observability backend writes structured JSONL events to a rotating file on disk. This is useful for environments without an external metrics/tracing stack — you get a persistent, greppable record of every agent lifecycle event.

### Enable

```toml
[observability]
backend = "file"
file_path = "state/file-trace.jsonl"
```

`file_path` is resolved relative to `workspace_dir`. Absolute paths are used as-is.

### File rotation

Rotated files are named `<stem>.YYYY-MM-DD.<seq>.<ext>`. Rotation is triggered by both file size and natural day boundary:

- **Size rotation**: when the active file reaches `max_file_size_mb`, it is renamed with the current date and a sequence number, and a new active file is created.
- **Date rotation**: on the first write of a new day, the previous day's file is renamed with its mtime date and a sequence number.

Configure the retention policy under `[observability.file_rotation]`:

```toml
[observability.file_rotation]
max_file_size_mb = 100      # rotate when file exceeds this size (default: 100 MB)
max_age_days = 30           # delete rotated files older than this (default: 30)
max_rotated_files = 100     # keep at most this many rotated files (default: 100)
```

Example rotation output:

```
state/file-trace.jsonl                          ← active file
state/file-trace.2026-05-12.1.jsonl             ← rotated by date (yesterday)
state/file-trace.2026-05-13.1.jsonl             ← rotated by size
state/file-trace.2026-05-13.2.jsonl             ← rotated by size again
```

### Event format

Each line is a JSON object with `id`, `timestamp`, `event_type`, and a `payload` object. Example:

```json
{"id":"a1b2c3","timestamp":"2026-05-13T10:15:30+08:00","event_type":"llm.response","payload":{"duration_ms":150,"success":true,"input_tokens":100,"output_tokens":50}}
```

### Querying

```bash
# All events
cat state/file-trace.jsonl | jq .

# Failed LLM calls
grep '"llm.response"' state/file-trace.jsonl | jq 'select(.payload.success == false)'

# Tool calls sorted by duration
grep '"tool.call"' state/file-trace.jsonl | jq -s 'sort_by(.payload.duration_ms) | reverse'
```

### Durability

The writer is **best-effort**: `sync_data()` is called after each write by default (`sync_on_write = true`), but write failures (disk full, permission denied, failed rotation) are logged as warnings and do not propagate to callers. `append()` is synchronous and uses `try_send` — if the internal channel is full, `Err(ChannelFull)` is returned and the event is dropped. `flush()` and `shutdown()` guarantee that all in-flight commands have been *processed* by the backend task — they do not guarantee that every write reached disk successfully. For observability data this trade-off is appropriate: a transient I/O error should not halt the agent loop.

## Receipts audit log

Separate from the general logs, tool receipts are written to:

```
<workspace>/receipts/<yyyy-mm-dd>.ndjson

The daemon's stderr formatter prefixes every line with the closest
enclosing alias-bound identity:

- agent context → `[<agent_alias>]`
- channel-only context (channel listener, no agent yet) → `[<channel_composite>]` (e.g. `[discord.glados]`)
- otherwise → `[system]`

The span chain follows: `channel_listener{channel=discord.glados}: …`.
Span fields are visible inline.

## Schema migration

On startup, if `log_persistence` is enabled and the file exists, the
writer streams any schema-1 rows through an in-place migration to
schema-2 before the first append. Pure streaming — bounded by a
single line's allocation regardless of file size. The migrated file is
atomically renamed into place. Files already at v2 are left untouched.

If migration fails, the daemon logs a `warn` and continues writing v2
appends; the old v1 rows remain readable by tools that still
understand v1 but won't pass the v2 reader's deserializer.

## What is `internal`?

`event.category = "internal"` is the bucket for ops noise an operator
doesn't need on the dashboard by default: heartbeat ticks, idle
broadcasts, lossy sync retries, and the like. The dashboard's "Hide
internal" toggle (on by default) filters these.

Use it when you have a high-frequency event whose presence matters for
forensics but whose absence is the normal state. Don't use it as a
volume governor for genuine errors.

## Files of interest

- `crates/zeroclaw-log/src/event.rs` — the canonical `LogEvent` shape.
- `crates/zeroclaw-log/src/layer.rs` — the `tracing-subscriber` Layer
  that captures every `tracing::*` call and feeds the pipeline.
- `crates/zeroclaw-log/src/macro.rs` — `record!`, `scope!`, `spawn!`.
- `crates/zeroclaw-log/src/writer.rs` — append + rolling trim.
- `crates/zeroclaw-log/src/reader.rs` — `/api/logs` reader.
- `crates/zeroclaw-log/src/config.rs` — `StoragePolicy`, `ToolIoPolicy`,
  `ResolvedPolicy`.
- `crates/zeroclaw-log/src/migrate.rs` — schema-1 → schema-2 streaming
  migration.
- `crates/zeroclaw-log/src/observer_bridge.rs` — typed `Observer`
  projection for Prometheus / OTel consumers.
- `crates/zeroclaw-gateway/src/api_logs.rs` — the HTTP adapter.

Touch the source before you trust the prose on this page.
