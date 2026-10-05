# Automated BS filter for Trump Truth Social posts

Polls Trump's Truth Social posts, rewrites each one in a style (currently
`five_year_old`: Trump as a five-year-old) through the Claude Code CLI, and
posts the rewrites to Bluesky as [@no-bs-trump.bsky.social](https://bsky.app/profile/no-bs-trump.bsky.social).

```
Truth Social API ──┐                       ┌─> Bluesky
  (mirror fallback)├─> SQLite ─> claude -p ─┤
trumpstruth.org ───┘   truths     rewrites  └─> Mastodon (optional)
```

One daemon loop every `poll_interval_secs`: **fetch** new posts, **transform**
the ones without a rewrite, **publish** the ones without a publication. Every
stage is idempotent against the database, so a crash or restart resumes where
it left off.

## Layout

| Path | What |
|---|---|
| `src/fetcher.rs` | Truth Social API (rustls only, the edge blocks OpenSSL) and the trumpstruth.org RSS mirror as fallback. Both key posts by the canonical Truth Social URL. |
| `src/transformer.rs` | Pipelines loaded from `prompts/*.toml`: model, effort, stages, optional audit stage, output sanitizing, refusal filter. |
| `src/claude.rs` | Runs `claude -p` per stage. Auth failures pause transforms with backoff; 429s sleep until the reset. |
| `src/publisher/` | Bluesky (threads over 300 chars, rollback on partial failure, re-login on lost session) and Mastodon. |
| `prompts/` | Git submodule (private) with the style prompts. |
| `scripts/check-ingest-health.py` | External health check, see below. |
| `deploy/truenas-app.yaml` | TrueNAS "Install via YAML" app. |

## Configuration

Copy `config.example.toml` to `config.toml` (gitignored; it holds the Bluesky
app password). `prompts.active` selects pipelines; publishers are enabled by
having a `[publishers.*]` section.

Claude auth: the CLI's own login (`claude auth login`) on a desktop, or
`CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token` in containers. Calls run
with `--setting-sources project --no-session-persistence`, so user plugins and
hooks do not apply and nothing is written to `~/.claude/projects`.

## Commands

```sh
non-violent-trump [--config config.toml] <command>
```

`daemon` (run forever), `once` (fetch + transform + publish), `fetch`,
`transform`, `publish`, `dry-run` (fetch + transform, no publishing),
`backfill` (scrape the full archive), `recover` (fill gaps backfill missed),
`sanitize` (re-apply output cleanup to stored rewrites, no model calls).

`--config` goes before the subcommand.

## Running

**Containers** (`docker compose up -d`): `headroom` (token-saving proxy for the
Claude API), `daemon`, and `health`. Needs `config.toml`, `prompts/`, `data/`,
`headroom/` next to the compose file and a `.env` with
`CLAUDE_CODE_OAUTH_TOKEN` and `NTFY_URL`. The image is published to
`ghcr.io/tuschla/trump-bs-filter` by CI on every push to `main`.

**TrueNAS**: Apps -> Discover -> Install via YAML with
`deploy/truenas-app.yaml`; the steps are in its header. Stop any other running
instance first, or posts get published twice.

**Desktop (systemd user units)**: `trump-daemon.service` runs
`target/release/non-violent-trump daemon` with
`ANTHROPIC_BASE_URL=http://localhost:8787` and `Requires=headroom-proxy.service`;
`trump-health.timer` runs the health check every 10 minutes. Install headroom
with `uv tool install "headroom-ai[proxy]"`; the AUR package lacks the proxy
dependencies.

## Health check

`scripts/check-ingest-health.py` runs outside the daemon and exits 0/1/2
(ok/warning/critical). It reports CRITICAL when a post from the mirror feed is
missing from the database, a stored post has no rewrite, or a rewrite is
unpublished, each past a 20-minute grace period. A Truth Social API 429 is a
WARNING (the mirror fallback still works). Alerts go to the journal,
`notify-send`, and ntfy (`NTFY_URL`), and push only when the state changes.

## Tests

```sh
cargo test
```
