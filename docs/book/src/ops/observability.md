# Logs & Observability

ZeroClaw emits structured logs, Prometheus metrics, and OpenTelemetry traces. All three are on by default in a release build.

## Logs

### Where they go

| Platform | Default destination |
|---|---|
| Linux (systemd) | journald — `journalctl --user -u zeroclaw` |
| macOS (launchd) | `~/Library/Logs/ZeroClaw/zeroclaw.log` (stdout), `zeroclaw.err` (stderr) |
| Homebrew on macOS | `$HOMEBREW_PREFIX/var/log/zeroclaw.log` |
| Windows | `%LOCALAPPDATA%\ZeroClaw\logs\zeroclaw.log` |
| Docker | container stdout — `docker logs zeroclaw` |
| Foreground (`zeroclaw daemon` without service) | stderr |

### Levels

Set via the `RUST_LOG` env var. Examples:

```bash
RUST_LOG=zeroclaw=info                       # default — high-signal events
RUST_LOG=zeroclaw=debug                      # verbose — per tool call, per provider call
RUST_LOG=zeroclaw::agent=trace               # very verbose — just the agent loop
RUST_LOG=warn,zeroclaw::security=debug       # quiet except security subsystem
```

For persistent changes, put the value in your service unit:

```ini
# ~/.config/systemd/user/zeroclaw.service.d/override.conf
[Service]
Environment=RUST_LOG=zeroclaw=info,zeroclaw::security=debug
```

### Format

JSON by default in service mode (easier for Loki/ELK ingestion). Pretty-print on a TTY (interactive `zeroclaw daemon`).

Force one or the other:

```toml
[observability]
log_format = "json"        # or "pretty"
```

### What's logged at `info`

- Service lifecycle (start, stop, config reload)
- Channel connect / disconnect events
- Provider calls with latency and token counts
- Tool calls (name, outcome — success / blocked / denied) — *not* arguments or results, which are sensitive
- Approval requests, grants, denials

At `debug`:

- Full tool args and results (redacted — credentials stripped)
- Every streaming chunk
- Memory retrieval scores
- Security-policy evaluation details

At `trace`:

- Model context window construction
- Per-token streaming events
- All HTTP request/response headers (still credential-redacted)

### Credential redaction

The logger redacts known secret patterns (`sk-*`, `ghp_*`, `xox[baprs]-*`, `ya29.*`, `AIza*`, etc.) regardless of log level. Redaction happens at the logger layer — your log files and the journal never see them.

Audit this with:

```bash
grep -E 'sk-|ghp_|xox[baprs]' /path/to/log/file | head
# should return zero results in a normal run
```

If you see an unredacted secret, file an issue — the redaction list is in `crates/zeroclaw-infra/src/redact.rs`.

## Metrics

Prometheus exposition on the gateway:

```
curl -s http://localhost:42617/metrics
```

Key metrics:

| Metric | Labels | What it measures |
|---|---|---|
| `zeroclaw_provider_calls_total` | `provider`, `outcome` | Provider calls by outcome (ok / timeout / error) |
| `zeroclaw_provider_latency_ms` | `provider` | Histogram of provider call latency |
| `zeroclaw_tokens_total` | `provider`, `kind` (input/output) | Token counters |
| `zeroclaw_tool_calls_total` | `tool`, `outcome` | Tool invocations by outcome |
| `zeroclaw_tool_duration_ms` | `tool` | Tool-execution histogram |
| `zeroclaw_channel_events_total` | `channel`, `direction` (inbound/outbound) | Message flow |
| `zeroclaw_channel_errors_total` | `channel`, `kind` | Disconnects, rate limits, auth failures |
| `zeroclaw_memory_searches_total` |  | Memory retrieval calls |
| `zeroclaw_policy_blocks_total` | `policy`, `tool` | Security-policy denials |

A minimal Prometheus scrape config:

```yaml
scrape_configs:
  - job_name: zeroclaw
    static_configs:
      - targets: ["localhost:42617"]
```

The [Grafana dashboard](https://github.com/zeroclaw-labs/zeroclaw-templates/tree/master/grafana) (in the templates repo) visualises the above.

## Traces

OpenTelemetry over OTLP/HTTP. Off by default — enable in config:

```toml
[observability.otel]
enabled = true
endpoint = "http://localhost:4318"
service_name = "zeroclaw"
```

Spans you'll see:

- `agent.loop` — one per inbound message, covers the whole turn
- `provider.chat` — a provider call, with attributes for model, token counts, and retry count
- `tool.invoke` — a tool call, with outcome and duration
- `security.validate` — policy check with decision
- `memory.search` — retrieval with hit count and max score

Pair with Jaeger or Tempo for distributed tracing if you run multiple ZeroClaw instances or are instrumenting downstream services.

## File trace backend

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

Writes are `sync_data()`-flushed by default (`sync_on_write = true` internally). This ensures events survive process crashes at the cost of throughput. The file observer also supports an explicit `flush()` that drains all in-flight writes before returning — used during graceful shutdown to guarantee no events are lost.

## Receipts audit log

Separate from the general logs, tool receipts are written to:

```
<workspace>/receipts/<yyyy-mm-dd>.ndjson
```

One JSON line per tool invocation. Greppable, append-only, persistent across restarts. See [Tool receipts](../security/tool-receipts.md).

## Cost & token tracking

Every gateway-served turn (WebSocket chat, channel webhook, simple webhook) records its token usage and computed cost to:

```
<workspace>/state/costs.jsonl
```

One JSON line per LLM call, with input/output token counts and cost in USD. The accumulator behind `GET /api/cost` reads from the same source — call it any time for a session, daily, and monthly summary.

Per-model pricing comes from `[cost.prices]` in `config.toml`. Models without an entry record token counts with a zero cost; budget enforcement still works on the recorded counts. The WebSocket `done` frame echoes the same numbers so browser clients can show usage without a second round-trip:

```json
{
  "type": "done",
  "full_response": "...",
  "input_tokens": 142,
  "output_tokens": 87,
  "tokens_used": 229,
  "cost_usd": 0.000456,
  "model": "claude-sonnet-4-20250514",
  "provider": "anthropic"
}
```

Token fields are `null` when the upstream provider does not surface usage in streaming responses (most OpenAI-compatible providers do; Anthropic streaming reports usage too, but only via its native event format — non-OpenAI-shaped providers may not).

## Health endpoints

The gateway exposes three health views:

```bash
curl -s http://localhost:42617/health              # { "status": "ok", "version": "0.7.5" }
curl -s http://localhost:42617/health/channels     # per-channel status
curl -s http://localhost:42617/health/providers    # per-provider status + error rate
```

Point Uptime Kuma / your monitor at `/health` for a binary liveness check.

## Log retention

ZeroClaw doesn't rotate its own logs — the OS handles that:

- systemd-journald rotates by size and age (`/etc/systemd/journald.conf`)
- launchd / macOS ASL rotates daily by default
- Docker's default `json-file` driver rotates at 10 MB with 3 retained files; configure in daemon.json

For the receipts log, rotate manually or via logrotate if the file grows faster than you want to retain:

```
<workspace>/receipts/2026-04-25.ndjson
<workspace>/receipts/2026-04-26.ndjson
...
```

Day-sharded means you can drop old files individually.

## See also

- [Operations → Overview](./overview.md)
- [Troubleshooting](./troubleshooting.md)
- [Security → Tool receipts](../security/tool-receipts.md)
