"""Generate and verify the deterministic mixed-format release fixture.

The fixture contains exactly 100 discovered logical sources and exactly 1,000
distinct canonical ``(client, session)`` identities after production
deduplication. It exercises ordinary Claude/Codex JSONL, streaming zstd Codex,
one schema-v1 retained session archive (SQLite/WAL), and one supported Codex
local-usage ledger (SQLite/WAL).

The upstream acceptance plan's illustrative distribution assumed one archive
SQLite source per group of sessions. The production contract admits exactly one
``session-usage-archive.sqlite`` and one ``codex-local-usage.sqlite`` per legacy
shared-data root, and retained archive periods do not create canonical request
sessions. The distribution below therefore preserves the required totals:
98 JSONL/zstd sources supply 98 canonical sessions and the single local-usage
ledger supplies the remaining 902 after one suppressed native-duplicate thread.
The archive source contributes retained history without fabricating request
rows.

Only synthetic values are emitted. No credential, prompt, title, cwd, real user
path, or copied user data is written. Compressed payloads are produced by the
reviewed ``zstd`` CLI with fixed arguments; it is an acceptance-harness
prerequisite, never a packaged runtime dependency.

SQLite WAL and shared-memory sidecars embed runtime-generated salts and index
headers, so their bytes differ between otherwise identical fixtures. All source
payloads and main databases are deterministic; the aggregate ``fixtureSha256``
therefore covers deterministic payloads only, while each sidecar is still
pinned by its per-file hash in the manifest.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import sqlite3
import subprocess
import sys
from typing import Any, Callable


SCHEMA = "token-monitor-lite-acceptance-v2"
REVISION = "mixed-2026-11-13.1"
SEED = 2062
CLAUDE_SOURCES = 33
CODEX_SOURCES = 33
ZSTD_SOURCES = 32
LOCAL_USAGE_SOURCES = 1
ARCHIVE_SOURCES = 1
SOURCE_COUNT = (
    CLAUDE_SOURCES + CODEX_SOURCES + ZSTD_SOURCES + LOCAL_USAGE_SOURCES + ARCHIVE_SOURCES
)
OBSERVATIONS_PER_JSONL = 10
LOCAL_THREADS = 903
LOCAL_ROWS_PER_THREAD = 2
SUPPRESSED_THREAD = "codex-fixture-000"
CANONICAL_SESSIONS = 1_000
CANONICAL_OBSERVATIONS = 2_784
ARCHIVE_SESSIONS = 40
MAX_FIXTURE_BYTES = 64 * 1024 * 1024
ZSTD_ARGS = ["-19", "-q", "-f"]


def _json_line(value: dict[str, Any]) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"


def _counts() -> dict[str, int]:
    return {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "reasoning": 0}


def _add_counts(target: dict[str, int], other: dict[str, int]) -> None:
    for key in target:
        target[key] += other[key]


def _claude_source(index: int) -> tuple[str, bytes, dict[str, int], int]:
    session = f"claude-fixture-{index:03d}"
    records: list[bytes] = []
    totals = _counts()
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 100 + index * 10 + observation,
            "output": 20 + observation,
            "cacheRead": 5 + observation % 3,
            "cacheWrite": 2 + observation % 2,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        records.append(
            _json_line(
                {
                    "message": {
                        "id": f"claude-message-{index:03d}-{observation:02d}",
                        "model": "claude-sonnet-4-5",
                        "usage": {
                            "cache_creation_input_tokens": counts["cacheWrite"],
                            "cache_read_input_tokens": counts["cacheRead"],
                            "input_tokens": counts["input"],
                            "output_tokens": counts["output"],
                        },
                    },
                    "requestId": f"claude-request-{index:03d}-{observation:02d}",
                    "sessionId": session,
                    "timestamp": f"2026-11-13T10:{index % 60:02d}:{observation:02d}Z",
                    "type": "assistant",
                }
            )
        )
    return session, b"".join(records), totals, len(records)


def _codex_source(index: int, session: str) -> tuple[str, bytes, dict[str, int], int]:
    records = [
        _json_line(
            {
                "payload": {"id": session, "model_provider": "openai", "source": "cli"},
                "type": "session_meta",
            }
        ),
        _json_line(
            {
                "payload": {"model": "gpt-5"},
                "timestamp": f"2026-11-13T11:{index % 60:02d}:00Z",
                "type": "turn_context",
            }
        ),
    ]
    totals = _counts()
    cumulative = {
        "input_tokens": 0,
        "output_tokens": 0,
        "cached_input_tokens": 0,
        "reasoning_output_tokens": 0,
    }
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 200 + index * 10 + observation,
            "output": 30 + observation,
            "cacheRead": 10 + observation % 4,
            "cacheWrite": 0,
            "reasoning": 5 + observation % 2,
        }
        _add_counts(totals, counts)
        last = {
            "input_tokens": counts["input"] + counts["cacheRead"],
            "output_tokens": counts["output"] + counts["reasoning"],
            "cached_input_tokens": counts["cacheRead"],
            "reasoning_output_tokens": counts["reasoning"],
        }
        for key, value in last.items():
            cumulative[key] += value
        records.append(
            _json_line(
                {
                    "payload": {
                        "info": {
                            "last_token_usage": last,
                            "total_token_usage": dict(cumulative),
                        },
                        "type": "token_count",
                    },
                    "timestamp": f"2026-11-13T11:{index % 60:02d}:{observation + 1:02d}Z",
                    "type": "event_msg",
                }
            )
        )
    return session, b"".join(records), totals, OBSERVATIONS_PER_JSONL


def _local_usage_row(thread_ordinal: int, checkpoint: int) -> tuple[int, int, int, int, int]:
    return (
        400 + thread_ordinal * 3 + checkpoint,
        40 + checkpoint,
        15 + thread_ordinal % 5,
        0,
        0,
    )


def _local_usage_database(
    path: Path,
) -> tuple[dict[str, int], int, dict[str, int], int]:
    totals = _counts()
    suppressed = _counts()
    observations = 0

    def build(connection: sqlite3.Connection) -> None:
        nonlocal observations
        connection.executescript(
            """
            CREATE TABLE threads (
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                metadata TEXT NOT NULL,
                checkpoint TEXT NOT NULL,
                PRIMARY KEY (account_key, thread_id)
            ) STRICT;
            CREATE TABLE requests (
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                checkpoint_total INTEGER NOT NULL,
                observed_at TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                model TEXT NOT NULL,
                cwd TEXT NOT NULL,
                usage TEXT NOT NULL,
                PRIMARY KEY (account_key, thread_id, checkpoint_total)
            ) STRICT;
            CREATE INDEX requests_thread ON requests (thread_id, observed_at);
            CREATE TABLE observer (
                singleton INTEGER PRIMARY KEY,
                owner TEXT NOT NULL,
                pid INTEGER NOT NULL,
                expires_at TEXT NOT NULL
            ) STRICT;
            CREATE TABLE agent_observer_intent (
                singleton INTEGER PRIMARY KEY,
                pid INTEGER NOT NULL,
                expires_at TEXT NOT NULL
            ) STRICT;
            """
        )
        thread_ids = [SUPPRESSED_THREAD] + [
            f"codex-local-{ordinal:03d}" for ordinal in range(LOCAL_THREADS - 1)
        ]
        for thread_ordinal, thread_id in enumerate(thread_ids):
            connection.execute(
                "INSERT INTO threads (account_key, thread_id, metadata, checkpoint) "
                "VALUES (?, ?, ?, ?)",
                ("account-fixture", thread_id, '{"nativeBacked":false}', "{}"),
            )
            for checkpoint in range(1, LOCAL_ROWS_PER_THREAD + 1):
                input_tokens, output_tokens, cache_read, cache_write, reasoning = (
                    _local_usage_row(thread_ordinal, checkpoint)
                )
                usage = {
                    "input": input_tokens,
                    "output": output_tokens,
                    "cacheRead": cache_read,
                    "cacheWrite": cache_write,
                    "reasoning": reasoning,
                    "total": input_tokens + output_tokens + cache_read + cache_write,
                }
                counts = {
                    "input": input_tokens,
                    "output": output_tokens,
                    "cacheRead": cache_read,
                    "cacheWrite": cache_write,
                    "reasoning": reasoning,
                }
                _add_counts(totals, counts)
                if thread_id == SUPPRESSED_THREAD:
                    _add_counts(suppressed, counts)
                observations += 1
                connection.execute(
                    "INSERT INTO requests "
                    "(account_key, thread_id, checkpoint_total, observed_at, turn_id, "
                    "model, cwd, usage) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        "account-fixture",
                        thread_id,
                        checkpoint,
                        f"2026-11-13T12:{thread_ordinal % 60:02d}:{checkpoint:02d}Z",
                        f"turn-{thread_ordinal:04d}-{checkpoint}",
                        "gpt-5-codex",
                        "synthetic",
                        json.dumps(usage, sort_keys=True, separators=(",", ":")),
                    ),
                )

    _build_wal_database(path, build)
    return totals, observations, suppressed, LOCAL_THREADS - 1


def _archive_entry(index: int) -> bytes:
    periods: dict[str, Any] = {}
    for period_ordinal, period in enumerate(("today", "month", "allTime")):
        input_tokens = 1_000 + index * 10 + period_ordinal
        output_tokens = 500 + period_ordinal
        cache_read = 100 + period_ordinal
        cache_write = 50
        total = input_tokens + output_tokens + cache_read + cache_write
        periods[period] = {
            "totalTokens": total,
            "inputTokens": input_tokens,
            "outputTokens": output_tokens,
            "cacheReadTokens": cache_read,
            "cacheWriteTokens": cache_write,
            "models": {"claude-sonnet-4-5": total},
            "modelTokenComponents": {
                "claude-sonnet-4-5": {
                    "inputTokens": input_tokens,
                    "outputTokens": output_tokens,
                    "cacheReadTokens": cache_read,
                    "cacheWriteTokens": cache_write,
                }
            },
        }
    return json.dumps(
        {
            "client": "claude",
            "sessionId": f"archive-session-{index:03d}",
            "capturedAt": "2026-11-12T12:00:00Z",
            "day": "2026-11-12",
            "month": "2026-11",
            "periods": periods,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def _create_archive_tables(connection: sqlite3.Connection, schema_version: str) -> None:
    connection.executescript(
        """
        CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
        CREATE TABLE sessions (
            session_key TEXT PRIMARY KEY,
            entry_json BLOB NOT NULL,
            revision INTEGER NOT NULL
        ) STRICT;
        CREATE INDEX sessions_revision_idx ON sessions (revision, session_key);
        """
    )
    connection.executemany(
        "INSERT INTO metadata (key, value) VALUES (?, ?)",
        [
            ("schema-version", schema_version),
            ("legacy-migrated", "1"),
            ("revision", "1"),
        ],
    )
    for index in range(ARCHIVE_SESSIONS):
        connection.execute(
            "INSERT INTO sessions (session_key, entry_json, revision) VALUES (?, ?, ?)",
            (f"archive-key-{index:03d}", _archive_entry(index), 1),
        )


def _archive_database(path: Path, schema_version: str = "1") -> None:
    connection = sqlite3.connect(path)
    try:
        _create_archive_tables(connection, schema_version)
        connection.commit()
    finally:
        connection.close()


def _archive_database_wal(path: Path) -> tuple[int, list[Path]]:
    def build(connection: sqlite3.Connection) -> None:
        _create_archive_tables(connection, "1")

    sidecars = _build_wal_database(path, build)
    return ARCHIVE_SESSIONS, sidecars


def _build_wal_database(
    target: Path,
    build: Callable[[sqlite3.Connection], None],
) -> list[Path]:
    for suffix in ("", "-wal", "-shm"):
        Path(f"{target}{suffix}").unlink(missing_ok=True)
    building = target.with_name(target.name + ".building")
    for suffix in ("", "-wal", "-shm"):
        Path(f"{building}{suffix}").unlink(missing_ok=True)
    connection = sqlite3.connect(building)
    try:
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute("PRAGMA synchronous=FULL")
        connection.execute("PRAGMA wal_autocheckpoint=0")
        build(connection)
        connection.commit()
        wal = Path(f"{building}-wal")
        shm = Path(f"{building}-shm")
        if not wal.is_file() or wal.stat().st_size == 0 or not shm.is_file():
            raise ValueError("generated SQLite WAL did not retain committed pages")
        shutil.copyfile(building, target)
        shutil.copyfile(wal, Path(f"{target}-wal"))
        shutil.copyfile(shm, Path(f"{target}-shm"))
    finally:
        connection.close()
        for suffix in ("", "-wal", "-shm"):
            Path(f"{building}{suffix}").unlink(missing_ok=True)
    return [Path(f"{target}-wal"), Path(f"{target}-shm")]


def _zstd_tool() -> tuple[str, str]:
    executable = os.environ.get("ZSTD") or shutil.which("zstd")
    if not executable:
        raise ValueError(
            "zstd CLI is required for acceptance fixture generation; "
            "it is a harness prerequisite, not a runtime dependency"
        )
    result = subprocess.run(
        [executable, "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode:
        raise ValueError("zstd CLI is unavailable")
    version = (result.stdout or result.stderr).decode("utf-8", "replace").strip()
    return executable, version


def _compress(executable: str, source: Path, target: Path) -> None:
    result = subprocess.run(
        [executable, *ZSTD_ARGS, str(source), "-o", str(target)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode or not target.is_file():
        raise ValueError("zstd compression failed")


def _validate_output_argument(raw: str) -> Path:
    candidate = Path(raw)
    if not candidate.is_absolute() or ".." in candidate.parts:
        raise ValueError("output root must be an absolute path without parent traversal")
    if candidate.is_symlink():
        raise ValueError("output root must not be a symlink")
    if candidate.exists() and any(candidate.iterdir()):
        raise ValueError("output root must not already contain files")
    candidate.mkdir(parents=True, exist_ok=True)
    return candidate.resolve(strict=True)


def _safe_path(root: Path, relative: str) -> Path:
    parsed = PurePosixPath(relative)
    if parsed.is_absolute() or not parsed.parts or ".." in parsed.parts:
        raise ValueError("fixture relative path is invalid")
    target = root.joinpath(*parsed.parts)
    target.parent.mkdir(parents=True, exist_ok=True)
    resolved_parent = target.parent.resolve(strict=True)
    if resolved_parent != root and root not in resolved_parent.parents:
        raise ValueError("fixture path escaped output root")
    return target


def _file_entry(root: Path, relative: str, data: bytes) -> dict[str, Any]:
    target = _safe_path(root, relative)
    with target.open("xb") as handle:
        handle.write(data)
        handle.flush()
        os.fsync(handle.fileno())
    return {
        "bytes": len(data),
        "path": relative,
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def _existing_entry(root: Path, relative: str, target: Path) -> dict[str, Any]:
    data = target.read_bytes()
    resolved = target.resolve(strict=True)
    if root != resolved and root not in resolved.parents:
        raise ValueError("fixture path escaped output root")
    return {
        "bytes": len(data),
        "path": relative,
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def _scenario_files(root: Path) -> list[dict[str, Any]]:
    session = "scenario-claude"
    base = _json_line(
        {
            "message": {"id": "base", "model": "claude", "usage": {"input_tokens": 7}},
            "sessionId": session,
            "timestamp": "2026-11-09T12:00:00Z",
            "type": "assistant",
        }
    )
    replacement = _json_line(
        {
            "message": {"id": "base", "model": "claude", "usage": {"input_tokens": 5}},
            "sessionId": session,
            "timestamp": "2026-11-09T12:00:00Z",
            "type": "assistant",
        }
    )
    appended = _json_line(
        {
            "message": {"id": "append", "model": "claude", "usage": {"input_tokens": 3}},
            "sessionId": session,
            "timestamp": "2026-11-09T12:00:01Z",
            "type": "assistant",
        }
    )
    large = _json_line(
        {
            "message": {"id": "large", "model": "claude", "usage": {"input_tokens": 1}},
            "padding": "x" * (256 * 1024),
            "sessionId": session,
            "timestamp": "2026-11-09T12:00:02Z",
            "type": "assistant",
        }
    )
    definitions = [
        ("scenarios/append-initial.jsonl", base),
        ("scenarios/append-record.jsonl", appended),
        ("scenarios/incomplete-tail.jsonl", appended[:-1]),
        ("scenarios/rewrite-before.jsonl", base),
        ("scenarios/rewrite-after.jsonl", replacement),
        ("scenarios/rewrite-grow-before.jsonl", base),
        ("scenarios/rewrite-grow-after.jsonl", replacement + appended),
        ("scenarios/rotation-before.jsonl", base),
        ("scenarios/rotation-after.jsonl", b"\n"),
        ("scenarios/source-error.jsonl", b"{malformed\n"),
        ("scenarios/large-record.jsonl", large),
    ]
    scenarios = [
        dict(_file_entry(root, path, data), role=Path(path).name.split(".")[0])
        for path, data in definitions
    ]

    executable, _version = _zstd_tool()
    codex_scenario = _json_line(
        {
            "payload": {"id": "scenario-codex", "model_provider": "openai", "source": "cli"},
            "type": "session_meta",
        }
    ) + _json_line(
        {
            "payload": {"model": "gpt-5"},
            "timestamp": "2026-11-09T12:00:02Z",
            "type": "turn_context",
        }
    ) + _json_line(
        {
            "payload": {
                "info": {
                    "last_token_usage": {
                        "input_tokens": 12,
                        "output_tokens": 4,
                        "cached_input_tokens": 2,
                        "reasoning_output_tokens": 0,
                    },
                    "total_token_usage": {
                        "input_tokens": 12,
                        "output_tokens": 4,
                        "cached_input_tokens": 2,
                        "reasoning_output_tokens": 0,
                    },
                },
                "type": "token_count",
            },
            "timestamp": "2026-11-09T12:00:03Z",
            "type": "event_msg",
        }
    )
    plain = root / "scenarios" / "codex-valid.tmp"
    plain.parent.mkdir(parents=True, exist_ok=True)
    plain.write_bytes(codex_scenario)
    compressed = plain.with_suffix(".jsonl.zst")
    _compress(executable, plain, compressed)
    valid = compressed.read_bytes()
    plain.unlink()
    compressed.unlink()
    zstd_files = [
        ("scenarios/codex-valid.jsonl.zst", valid),
        ("scenarios/codex-truncated.jsonl.zst", valid[: max(1, len(valid) // 2)]),
        (
            "scenarios/codex-corrupt.jsonl.zst",
            valid[: len(valid) // 2]
            + bytes([valid[len(valid) // 2] ^ 0xFF])
            + valid[len(valid) // 2 + 1 :],
        ),
    ]
    scenarios.extend(
        dict(_file_entry(root, path, data), role=Path(path).name.split(".")[0])
        for path, data in zstd_files
    )

    unsupported = root / "scenarios" / "archive-unsupported.sqlite"
    _archive_database(unsupported, schema_version="2")
    scenarios.append(
        dict(
            _existing_entry(root, "scenarios/archive-unsupported.sqlite", unsupported),
            role="archive-unsupported",
        )
    )
    wrong_role = root / "scenarios" / "codex-local-usage-wrong-role.sqlite"
    _archive_database(wrong_role)
    scenarios.append(
        dict(
            _existing_entry(
                root, "scenarios/codex-local-usage-wrong-role.sqlite", wrong_role
            ),
            role="codex-local-usage-wrong-role",
        )
    )
    return scenarios


def _source_entry(
    root: Path,
    relative: str,
    data: bytes,
    *,
    format_name: str,
    session: str,
    observations: int,
    canonical_sessions: int,
    expected_totals: dict[str, int],
    physical_bytes: int,
    suppressed_observations: int = 0,
    suppressed_totals: dict[str, int] | None = None,
    retained_periods: int = 0,
) -> dict[str, Any]:
    entry = dict(
        _file_entry(root, relative, data),
        format=format_name,
        session=session,
        observations=observations,
        canonicalSessions=canonical_sessions,
        suppressedObservations=suppressed_observations,
        decodedBytes=len(data),
        physicalBytes=physical_bytes,
        expectedTotals=expected_totals,
        retainedPeriods=retained_periods,
    )
    if suppressed_totals is not None:
        entry["suppressedTotals"] = suppressed_totals
    return entry


def generate(raw_root: str) -> Path:
    root = _validate_output_argument(raw_root)
    sources: list[dict[str, Any]] = []
    bundles: list[dict[str, Any]] = []
    aggregate = _counts()
    canonical_aggregate = _counts()
    physical_bytes = 0
    decoded_bytes = 0
    zstd_executable, zstd_version = _zstd_tool()

    for index in range(CLAUDE_SOURCES):
        session, data, totals, rows = _claude_source(index)
        relative = f"home/.claude/projects/acceptance/{session}.jsonl"
        sources.append(
            _source_entry(
                root,
                relative,
                data,
                format_name="claude-cli",
                session=session,
                observations=rows,
                canonical_sessions=1,
                expected_totals=totals,
                physical_bytes=len(data),
            )
        )
        _add_counts(aggregate, totals)
        _add_counts(canonical_aggregate, totals)
        physical_bytes += len(data)
        decoded_bytes += len(data)

    for index in range(CODEX_SOURCES):
        session = f"codex-fixture-{index:03d}"
        session, data, totals, rows = _codex_source(index, session)
        relative = f"home/.codex/sessions/2026/11/{session}.jsonl"
        sources.append(
            _source_entry(
                root,
                relative,
                data,
                format_name="codex-cli",
                session=session,
                observations=rows,
                canonical_sessions=1,
                expected_totals=totals,
                physical_bytes=len(data),
            )
        )
        _add_counts(aggregate, totals)
        _add_counts(canonical_aggregate, totals)
        physical_bytes += len(data)
        decoded_bytes += len(data)

    for index in range(ZSTD_SOURCES):
        session = f"codex-zstd-{index:03d}"
        _session, data, totals, rows = _codex_source(1000 + index, session)
        plain = root / "home" / ".codex" / "sessions" / "2026" / "11" / f"{session}.tmp"
        plain.parent.mkdir(parents=True, exist_ok=True)
        plain.write_bytes(data)
        target = plain.with_suffix(".jsonl.zst")
        _compress(zstd_executable, plain, target)
        plain.unlink()
        compressed = target.read_bytes()
        relative = f"home/.codex/sessions/2026/11/{session}.jsonl.zst"
        sources.append(
            dict(
                _existing_entry(root, relative, target),
                format="codex-cli-zstd",
                session=session,
                observations=rows,
                canonicalSessions=1,
                suppressedObservations=0,
                decodedBytes=len(data),
                physicalBytes=len(compressed),
                expectedTotals=totals,
                retainedPeriods=0,
            )
        )
        _add_counts(aggregate, totals)
        _add_counts(canonical_aggregate, totals)
        physical_bytes += len(compressed)
        decoded_bytes += len(data)

    shared = root / "home" / ".config" / "Token Monitor"
    shared.mkdir(parents=True, exist_ok=True)
    local_target = shared / "codex-local-usage.sqlite"
    local_totals, local_observations, suppressed_totals, local_sessions = (
        _local_usage_database(local_target)
    )
    local_sidecars = [Path(f"{local_target}{suffix}") for suffix in ("-wal", "-shm")]
    local_entry = dict(
        _existing_entry(
            root, "home/.config/Token Monitor/codex-local-usage.sqlite", local_target
        ),
        format="codex-local-usage-sqlite",
        session="codex-local",
        observations=local_observations,
        canonicalSessions=local_sessions,
        suppressedObservations=LOCAL_ROWS_PER_THREAD,
        suppressedTotals=suppressed_totals,
        decodedBytes=local_target.stat().st_size,
        physicalBytes=local_target.stat().st_size,
        expectedTotals=local_totals,
        retainedPeriods=0,
    )
    sources.append(local_entry)
    _add_counts(aggregate, local_totals)
    _add_counts(canonical_aggregate, local_totals)
    _add_counts(
        canonical_aggregate, {key: -value for key, value in suppressed_totals.items()}
    )
    physical_bytes += local_target.stat().st_size
    decoded_bytes += local_target.stat().st_size
    local_bundle = {"main": local_entry["path"], "sidecars": []}
    for sidecar in local_sidecars:
        relative = sidecar.relative_to(root).as_posix()
        sidecar_entry = _existing_entry(root, relative, sidecar)
        local_bundle["sidecars"].append(sidecar_entry)
        physical_bytes += sidecar_entry["bytes"]
        decoded_bytes += sidecar_entry["bytes"]
    bundles.append(local_bundle)

    archive_target = shared / "session-usage-archive.sqlite"
    archive_sessions, archive_sidecars = _archive_database_wal(archive_target)
    archive_entry = dict(
        _existing_entry(
            root,
            "home/.config/Token Monitor/session-usage-archive.sqlite",
            archive_target,
        ),
        format="session-archive-sqlite",
        session="archive",
        observations=0,
        canonicalSessions=0,
        suppressedObservations=0,
        decodedBytes=archive_target.stat().st_size,
        physicalBytes=archive_target.stat().st_size,
        expectedTotals=_counts(),
        retainedPeriods=archive_sessions * 3,
    )
    sources.append(archive_entry)
    physical_bytes += archive_target.stat().st_size
    decoded_bytes += archive_target.stat().st_size
    archive_bundle = {"main": archive_entry["path"], "sidecars": []}
    for sidecar in archive_sidecars:
        relative = sidecar.relative_to(root).as_posix()
        sidecar_entry = _existing_entry(root, relative, sidecar)
        archive_bundle["sidecars"].append(sidecar_entry)
        physical_bytes += sidecar_entry["bytes"]
        decoded_bytes += sidecar_entry["bytes"]
    bundles.append(archive_bundle)

    scenarios = _scenario_files(root)
    for entry in scenarios:
        physical_bytes += entry["bytes"]
        decoded_bytes += entry["bytes"]

    generated_files = sources + scenarios
    fixture_hash = hashlib.sha256(
        "".join(
            f"{entry['path']}\0{entry['sha256']}\0{entry['bytes']}\n"
            for entry in sorted(generated_files, key=lambda entry: entry["path"])
        ).encode("utf-8")
    ).hexdigest()

    format_counts: dict[str, int] = {}
    for entry in sources:
        format_counts[entry["format"]] = format_counts.get(entry["format"], 0) + 1
    observations = sum(entry["observations"] for entry in sources)
    canonical_sessions = sum(entry["canonicalSessions"] for entry in sources)
    overlap_observations = sum(entry["suppressedObservations"] for entry in sources)
    retained_periods = sum(entry["retainedPeriods"] for entry in sources)
    if (
        len(sources) != SOURCE_COUNT
        or canonical_sessions != CANONICAL_SESSIONS
        or observations - overlap_observations != CANONICAL_OBSERVATIONS
        or retained_periods != ARCHIVE_SESSIONS * 3
    ):
        raise ValueError("generated fixture does not match its declared counts")

    manifest = {
        "schema": SCHEMA,
        "revision": REVISION,
        "seed": SEED,
        "compression": {"tool": "zstd", "version": zstd_version, "args": ZSTD_ARGS},
        "coverage": {
            "canonicalObservations": observations - overlap_observations,
            "canonicalSessions": canonical_sessions,
            "configuredSources": len(sources),
            "decodedBytes": decoded_bytes,
            "liveObservations": observations,
            "mixedCompressedSqlite": True,
            "overlapObservations": overlap_observations,
            "physicalBytes": physical_bytes,
            "retainedHistory": "present",
            "retainedObservations": 0,
            "retainedPeriods": retained_periods,
            "retainedSessions": archive_sessions,
            "sourceFormats": format_counts,
            "usageObservations": observations,
        },
        "releaseTarget": {
            "canonicalSessions": CANONICAL_SESSIONS,
            "configuredSources": SOURCE_COUNT,
            "met": True,
            "missing": [],
        },
        "expectedAggregate": canonical_aggregate,
        "expectedSourceAggregate": aggregate,
        "determinism": {
            "aggregateCovers": "source-and-scenario-payloads",
            "excludedFromAggregate": ["sqlite-wal", "sqlite-shm"],
            "reason": "SQLite WAL salts and shared-memory headers are runtime-generated",
        },
        "fixtureSha256": fixture_hash,
        "sources": sources,
        "bundles": bundles,
        "scenarios": scenarios,
    }
    manifest_bytes = json.dumps(manifest, indent=2, sort_keys=True).encode("utf-8") + b"\n"
    manifest_entry = _file_entry(root, "manifest.json", manifest_bytes)
    if physical_bytes + manifest_entry["bytes"] > MAX_FIXTURE_BYTES:
        raise ValueError("fixture exceeds byte budget")
    return root / "manifest.json"


def _regular_file(root: Path, relative: str) -> Path:
    parsed = PurePosixPath(relative)
    if parsed.is_absolute() or not parsed.parts or ".." in parsed.parts:
        raise ValueError("manifest contains an invalid relative path")
    path = root.joinpath(*parsed.parts)
    stat = path.lstat()
    if path.is_symlink() or not path.is_file():
        raise ValueError("fixture entry is not a regular file")
    if stat.st_size > MAX_FIXTURE_BYTES:
        raise ValueError("fixture entry exceeds byte budget")
    resolved = path.resolve(strict=True)
    if root != resolved and root not in resolved.parents:
        raise ValueError("fixture entry escaped root")
    return path


def _verify_entry(root: Path, entry: dict[str, Any], seen: set[str]) -> int:
    relative = entry.get("path")
    if not isinstance(relative, str) or relative in seen:
        raise ValueError("fixture path is missing or duplicated")
    seen.add(relative)
    path = _regular_file(root, relative)
    data = path.read_bytes()
    if len(data) != entry.get("bytes") or hashlib.sha256(data).hexdigest() != entry.get(
        "sha256"
    ):
        raise ValueError("fixture entry checksum mismatch")
    return len(data)


def verify(raw_root: str) -> dict[str, Any]:
    root = Path(raw_root)
    if not root.is_absolute() or ".." in root.parts or root.is_symlink():
        raise ValueError("fixture root must be an absolute non-symlink path")
    root = root.resolve(strict=True)
    manifest_path = _regular_file(root, "manifest.json")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("schema") != SCHEMA or manifest.get("revision") != REVISION:
        raise ValueError("fixture manifest schema or revision is unsupported")

    sources = manifest.get("sources")
    scenarios = manifest.get("scenarios")
    bundles = manifest.get("bundles")
    coverage = manifest.get("coverage")
    if (
        not isinstance(sources, list)
        or len(sources) != SOURCE_COUNT
        or not isinstance(scenarios, list)
        or not isinstance(bundles, list)
        or len(bundles) != 2
        or not isinstance(coverage, dict)
    ):
        raise ValueError("fixture entry counts are invalid")

    seen: set[str] = set()
    source_paths = {entry.get("path") for entry in sources}
    total_bytes = manifest_path.stat().st_size
    physical_bytes = 0
    decoded_bytes = 0
    aggregate = _counts()
    canonical_aggregate = _counts()
    format_counts: dict[str, int] = {}
    observations = 0
    canonical_sessions = 0
    overlap = 0
    retained_periods = 0
    for entry in sources:
        total_bytes += _verify_entry(root, entry, seen)
        expected = entry.get("expectedTotals")
        if not isinstance(expected, dict) or set(expected) != set(aggregate):
            raise ValueError("source totals are invalid")
        _add_counts(aggregate, expected)
        _add_counts(canonical_aggregate, expected)
        suppressed = int(entry.get("suppressedObservations", 0))
        if suppressed:
            suppressed_totals = entry.get("suppressedTotals")
            if not isinstance(suppressed_totals, dict):
                raise ValueError("suppressed source totals are invalid")
            _add_counts(
                canonical_aggregate, {key: -value for key, value in suppressed_totals.items()}
            )
        format_name = entry.get("format")
        if not isinstance(format_name, str):
            raise ValueError("source format is invalid")
        format_counts[format_name] = format_counts.get(format_name, 0) + 1
        observations += int(entry.get("observations", -1))
        canonical_sessions += int(entry.get("canonicalSessions", -1))
        overlap += suppressed
        retained_periods += int(entry.get("retainedPeriods", -1))
        physical_bytes += int(entry.get("physicalBytes", -1))
        decoded_bytes += int(entry.get("decodedBytes", -1))
    for entry in scenarios:
        total_bytes += _verify_entry(root, entry, seen)
        physical_bytes += entry["bytes"]
        decoded_bytes += entry["bytes"]
    for bundle in bundles:
        main = bundle.get("main")
        sidecars = bundle.get("sidecars")
        if main not in source_paths or not isinstance(sidecars, list) or not sidecars:
            raise ValueError("fixture SQLite sidecar bundle is malformed")
        for entry in sidecars:
            total_bytes += _verify_entry(root, entry, seen)
            physical_bytes += entry["bytes"]
            decoded_bytes += entry["bytes"]

    generated_files = sources + scenarios
    fixture_hash = hashlib.sha256(
        "".join(
            f"{entry['path']}\0{entry['sha256']}\0{entry['bytes']}\n"
            for entry in sorted(generated_files, key=lambda entry: entry["path"])
        ).encode("utf-8")
    ).hexdigest()

    determinism = manifest.get("determinism")
    if (
        not isinstance(determinism, dict)
        or determinism.get("excludedFromAggregate") != ["sqlite-wal", "sqlite-shm"]
    ):
        raise ValueError("fixture determinism contract is missing")

    if (
        coverage.get("configuredSources") != SOURCE_COUNT
        or coverage.get("canonicalSessions") != CANONICAL_SESSIONS
        or coverage.get("usageObservations") != observations
        or coverage.get("canonicalObservations") != observations - overlap
        or coverage.get("overlapObservations") != overlap
        or coverage.get("retainedPeriods") != retained_periods
        or coverage.get("sourceFormats") != format_counts
        or coverage.get("physicalBytes") != physical_bytes
        or coverage.get("decodedBytes") != decoded_bytes
        or canonical_sessions != CANONICAL_SESSIONS
        or observations - overlap != CANONICAL_OBSERVATIONS
    ):
        raise ValueError("fixture declared counts are inconsistent")
    if (
        aggregate != manifest.get("expectedSourceAggregate")
        or canonical_aggregate != manifest.get("expectedAggregate")
    ):
        raise ValueError("fixture aggregate is invalid")
    if manifest.get("fixtureSha256") != fixture_hash:
        raise ValueError("fixture aggregate hash mismatch")
    if total_bytes > MAX_FIXTURE_BYTES:
        raise ValueError("fixture exceeds byte budget")
    release_target = manifest.get("releaseTarget", {})
    if (
        release_target.get("met") is not True
        or release_target.get("canonicalSessions") != CANONICAL_SESSIONS
        or release_target.get("configuredSources") != SOURCE_COUNT
    ):
        raise ValueError("fixture release target is inconsistent")
    return {
        "canonicalObservations": observations - overlap,
        "canonicalSessions": canonical_sessions,
        "decodedBytes": decoded_bytes,
        "fixtureBytes": total_bytes,
        "fixtureSha256": fixture_hash,
        "manifestSha256": hashlib.sha256(manifest_path.read_bytes()).hexdigest(),
        "observations": observations,
        "physicalBytes": physical_bytes,
        "releaseTargetMet": True,
        "retainedPeriods": retained_periods,
        "schema": SCHEMA,
        "sources": SOURCE_COUNT,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--output", help="new or empty absolute output directory")
    action.add_argument("--verify", help="absolute fixture directory")
    arguments = parser.parse_args()
    try:
        if arguments.output:
            manifest = generate(arguments.output)
            result = verify(str(manifest.parent))
        else:
            result = verify(arguments.verify)
        print(json.dumps(result, sort_keys=True, separators=(",", ":")))
        return 0
    except (OSError, ValueError, sqlite3.Error, json.JSONDecodeError) as error:
        print(f"acceptance fixture error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
