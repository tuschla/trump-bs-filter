#!/usr/bin/env python3
"""Ingest/publish health check for the non-violent-trump daemon.

Runs from a timer, deliberately outside the daemon: a monitor that shares fate
with the process it watches is how a 19-hour ingest blackout stayed invisible
behind an "active (running)" unit.

Wall-clock staleness is NOT a usable signal here. Multi-hour silences are normal
for the account (a genuine 10.6h gap occurred on 2026-09-09), so "nothing stored
recently" cannot distinguish a quiet night from a broken fetch. Ground truth is
the public mirror feed instead: if a text-bearing post that is already older than
the grace window is missing from the database, ingest is broken no matter how
quiet the account has been.

Ingest and publish alone do not cover the stage in between: when every rewrite
fails (an expired Claude login did this for 8 days), nothing reaches the publish
check and both stay green. Posts left without a rewrite past the grace window
are checked directly for that reason.

Alerts go to the journal (logger), the desktop (notify-send) and ntfy
(--ntfy-url / NTFY_URL), whichever exist. Push alerts fire only when the overall
state changes, so a long outage is one notification plus one on recovery, not
one per timer tick.

Exit status: 0 healthy, 1 degraded (warning), 2 broken (critical).
"""

from __future__ import annotations

import argparse
import html
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from datetime import datetime, timedelta, timezone
from email.utils import parsedate_to_datetime
from pathlib import Path

TRUTH_NS = {"truth": "https://truthsocial.com/ns"}
BROWSER_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/120.0 Safari/537.36"
)
TAG_RE = re.compile(r"<[^>]+>")


def strip_tags(raw: str) -> str:
    return TAG_RE.sub("", html.unescape(raw or "")).strip()


def get(url: str, timeout: int) -> tuple[int, bytes]:
    """Return (status, body). A non-200 is returned rather than raised."""
    request = urllib.request.Request(
        url,
        headers={
            "User-Agent": BROWSER_UA,
            "Accept": "application/json, text/plain, */*",
            "Referer": "https://truthsocial.com/@realDonaldTrump",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as e:
        return e.code, b""


def feed_items(body: bytes) -> list[dict]:
    """Text-bearing feed items, newest first.

    Empty posts (media-only, bare reposts) are excluded because the daemon never
    stores them, so their absence is not a fault.
    """
    root = ET.fromstring(body)
    items = []
    for item in root.iterfind("./channel/item"):
        text = strip_tags(item.findtext("description", ""))
        if not text:
            continue
        published = item.findtext("pubDate")
        if not published:
            continue
        try:
            when = parsedate_to_datetime(published)
        except (TypeError, ValueError):
            continue
        if when.tzinfo is None:
            when = when.replace(tzinfo=timezone.utc)
        original = item.findtext("truth:originalUrl", namespaces=TRUTH_NS)
        items.append(
            {
                "when": when,
                "ids": [i for i in (original, item.findtext("link"), item.findtext("guid")) if i],
                "text": text,
            }
        )
    items.sort(key=lambda i: i["when"], reverse=True)
    return items


def notify_ntfy(url: str, token: str | None, label: str, summary: str, timeout: int) -> None:
    headers = {
        "Title": f"Trump pipeline {label.lower()}",
        "Priority": {"CRITICAL": "urgent", "WARNING": "default"}.get(label, "low"),
        "Tags": {"CRITICAL": "rotating_light", "WARNING": "warning"}.get(label, "white_check_mark"),
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, data=summary.encode(), headers=headers, method="POST")
    try:
        urllib.request.urlopen(request, timeout=timeout).close()
    except (urllib.error.URLError, TimeoutError) as e:
        print(f"WARNING: ntfy delivery failed: {e}", file=sys.stderr)


def state_changed(path: Path, label: str) -> bool:
    """Record `label` as the current state; True if it differs from the last run."""
    try:
        previous = json.loads(path.read_text()).get("label")
    except (OSError, ValueError):
        previous = None if label != "OK" else "OK"  # first run: only alert if unhealthy
    if previous != label:
        path.write_text(json.dumps({"label": label, "since": datetime.now(timezone.utc).isoformat()}))
        return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=Path(__file__).resolve().parent.parent / "config.toml")
    parser.add_argument(
        "--unit",
        default="trump-daemon.service",
        help="systemd user unit to check; skipped where systemctl is absent (containers)",
    )
    parser.add_argument(
        "--grace-minutes",
        type=int,
        default=20,
        help="how long a post may exist before absence counts as a fault",
    )
    parser.add_argument("--timeout", type=int, default=20)
    parser.add_argument("--no-notify", action="store_true")
    parser.add_argument("--ntfy-url", default=os.environ.get("NTFY_URL"), help="e.g. https://ntfy.sh/<topic>")
    parser.add_argument("--ntfy-token", default=os.environ.get("NTFY_TOKEN"))
    args = parser.parse_args()

    with args.config.open("rb") as fh:
        config = tomllib.load(fh)

    project = args.config.resolve().parent
    db_path = Path(config["storage"]["db_path"])
    if not db_path.is_absolute():
        db_path = project / db_path
    feed_url = config["feed"].get("url", "https://trumpstruth.org/feed")
    styles = config.get("prompts", {}).get("active") or []
    platforms = list(config.get("publishers", {}).keys())

    critical: list[str] = []
    warnings: list[str] = []

    # 1. The unit must actually be up. Cheap, and catches the blunt failures.
    #    In a container the runtime's restart policy owns this instead.
    if shutil.which("systemctl"):
        state = subprocess.run(
            ["systemctl", "--user", "is-active", args.unit],
            capture_output=True,
            text=True,
        ).stdout.strip()
        if state != "active":
            critical.append(f"{args.unit} is {state or 'unknown'}, not active")

    # 2. Ingest: is a post the mirror published, old enough to have been picked
    #    up, missing from our database?
    cutoff = datetime.now(timezone.utc) - timedelta(minutes=args.grace_minutes)
    status, body = get(feed_url, args.timeout)
    if status != 200 or not body:
        warnings.append(f"mirror feed unreachable (HTTP {status}); ingest not verified")
    else:
        try:
            items = feed_items(body)
        except ET.ParseError as e:
            items = []
            warnings.append(f"mirror feed did not parse ({e}); ingest not verified")

        ripe = [i for i in items if i["when"] <= cutoff]
        if ripe:
            newest = ripe[0]
            with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
                placeholders = ",".join("?" * len(newest["ids"]))
                stored = db.execute(
                    f"SELECT 1 FROM truths WHERE id IN ({placeholders}) LIMIT 1",
                    newest["ids"],
                ).fetchone()
            if not stored:
                age = int((datetime.now(timezone.utc) - newest["when"]).total_seconds() // 60)
                critical.append(
                    f"ingest behind: post from {age}m ago absent from db "
                    f"({newest['ids'][0]}): {newest['text'][:60]!r}"
                )

    if db_path.exists():
        with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
            for style in styles:
                # 3. Transform: a stored post that is still neither rewritten nor
                #    recorded as skipped past the grace window. The content filter
                #    mirrors Storage::get_untransformed; posts the Rust-side filter
                #    rejects land in transform_skips within one cycle.
                (pending,) = db.execute(
                    """SELECT COUNT(*) FROM truths t
                       WHERE NOT EXISTS (SELECT 1 FROM rewrites r
                                         WHERE r.truth_id = t.id AND r.style = ?)
                         AND NOT EXISTS (SELECT 1 FROM transform_skips s
                                         WHERE s.truth_id = t.id AND s.style = ?)
                         AND TRIM(t.content) != ''
                         AND NOT (TRIM(t.content) GLOB 'http*' AND INSTR(TRIM(t.content), ' ') = 0)
                         AND t.fetched_at <= datetime('now', ?)""",
                    (style, style, f"-{args.grace_minutes} minutes"),
                ).fetchone()
                if pending:
                    critical.append(
                        f"transform stalled: {pending} post(s) without a {style} rewrite "
                        f"for over {args.grace_minutes}m (check claude auth / daemon log)"
                    )

                # 4. Publish: a rewrite that exists but stayed unpublished past the
                #    grace window means the publish stage is stuck. Scoped to active
                #    styles and configured platforms so inactive ones stay quiet.
                for platform in platforms:
                    (stuck,) = db.execute(
                        """SELECT COUNT(*) FROM rewrites r
                           WHERE r.style = ?
                             AND NOT EXISTS (
                                 SELECT 1 FROM publications p
                                 WHERE p.truth_id = r.truth_id
                                   AND p.style = r.style
                                   AND p.platform = ?
                             )
                             AND r.created_at <= datetime('now', ?)""",
                        (style, platform, f"-{args.grace_minutes} minutes"),
                    ).fetchone()
                    if stuck:
                        critical.append(
                            f"publish stalled: {stuck} {style} rewrite(s) unpublished "
                            f"to {platform} for over {args.grace_minutes}m"
                        )

    # 5. Early warning: the API is the low-latency path. If it is blocked we are
    #    running on the mirror, which still works but costs ~160s of extra lag.
    account = config["feed"].get("truthsocial_account_id")
    if config["feed"].get("source") == "truthsocial" and account:
        api = (
            f"https://truthsocial.com/api/v1/accounts/{account}"
            "/statuses?exclude_replies=true&limit=1"
        )
        api_status, _ = get(api, args.timeout)
        if api_status != 200:
            warnings.append(
                f"Truth Social API returning HTTP {api_status}; running degraded "
                "on the mirror fallback (~160s extra latency)"
            )

    for message in critical:
        print(f"CRITICAL: {message}")
    for message in warnings:
        print(f"WARNING: {message}")

    if critical:
        priority, label = "err", "CRITICAL"
    elif warnings:
        priority, label = "warning", "WARNING"
    else:
        priority, label = "info", "OK"
    summary = "; ".join(critical + warnings) or "ingest, transform and publishing current"
    if label == "OK":
        print(f"OK: {summary}, API reachable")

    changed = state_changed(db_path.parent / "health-state.json", label)
    # stdout already carries the verdict; logger only adds a journal copy where
    # a syslog socket exists (not in a container).
    if label != "OK" and shutil.which("logger") and os.path.exists("/dev/log"):
        subprocess.run(
            ["logger", "-t", "trump-health", "-p", f"user.{priority}", f"{label}: {summary}"],
            check=False,
        )
    if changed and not args.no_notify:
        if shutil.which("notify-send"):
            subprocess.run(
                [
                    "notify-send",
                    "--app-name=trump-daemon",
                    f"--urgency={'critical' if critical else 'normal'}",
                    f"Trump pipeline {label.lower()}",
                    summary,
                ],
                check=False,
            )
        if args.ntfy_url:
            notify_ntfy(args.ntfy_url, args.ntfy_token, label, summary, args.timeout)

    return {"OK": 0, "WARNING": 1, "CRITICAL": 2}[label]


if __name__ == "__main__":
    sys.exit(main())
