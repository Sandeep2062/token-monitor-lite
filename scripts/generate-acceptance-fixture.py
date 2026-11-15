"""Generate and verify the deterministic mixed-format release fixture.

The fixture contains exactly 100 discovered logical sources and exactly 1,000
distinct canonical ``(client, session)`` identities after production
deduplication. It exercises ordinary Claude/Codex JSONL, streaming zstd Codex,
one OpenCode SQLite database (``session_v2``/``session_message`` current
shape), one schema-v1 retained session archive (SQLite/WAL), and one supported
Codex local-usage ledger (SQLite/WAL).

The upstream acceptance plan's illustrative distribution assumed one archive
SQLite source per group of sessions. The production contract admits exactly one
``session-usage-archive.sqlite`` and one ``codex-local-usage.sqlite`` per legacy
shared-data root, and retained archive periods do not create canonical request
sessions. The distribution below preserves the required totals: 20 Codex JSONL
and 32 zstd Codex sources plus one OpenCode, one Gemini, one Cursor, one Cline,
one Amp, one Kilo, one Goose, one Copilot, one CodeBuddy, one WorkBuddy, one
Pi, one OMP and one Senpi source supply 65 canonical
sessions, 33 Claude sources supply 33 (one carries nine extra observations so
the session-level Goose source does not reduce the observation total), and the
single local-usage ledger supplies the remaining 902 after one suppressed
native-duplicate thread. The archive source contributes retained history
without fabricating request rows.

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
REVISION = "mixed-2026-11-13.4"
SEED = 2062
CLAUDE_SOURCES = 19
CODEX_SOURCES = 14
ZSTD_SOURCES = 32
OPENCODE_SOURCES = 1
GEMINI_SOURCES = 1
CURSOR_SOURCES = 1
CLINE_SOURCES = 1
AMP_SOURCES = 1
KILO_SOURCES = 1
GOOSE_SOURCES = 1
HERMES_SOURCES = 1
OPENCLAW_SOURCES = 1
DROID_SOURCES = 1
KIMI_SOURCES = 1
GROK_SOURCES = 1
COMMANDCODE_SOURCES = 1
MIMO_SOURCES = 1
ZCODE_SOURCES = 1
KIRO_SOURCES = 1
PROMA_SOURCES = 1
QODERCN_SOURCES = 1
DSH_SOURCES = 1
CHERRYSTUDIO_SOURCES = 1
LMSTUDIO_SOURCES = 1
COPILOT_SOURCES = 1
CODEBUDDY_SOURCES = 1
WORKBUDDY_SOURCES = 1
PI_SOURCES = 1
OMP_SOURCES = 1
SENPI_SOURCES = 1
ANTIGRAVITY_SOURCES = 1
QWEN_SOURCES = 1
REASONIX_SOURCES = 1
MUSE_SOURCES = 1
MCODE_SOURCES = 1
ZED_SOURCES = 1
LOCAL_USAGE_SOURCES = 1
ARCHIVE_SOURCES = 1
EXTRA_CLAUDE_OBSERVATIONS = 27
SOURCE_COUNT = (
    CLAUDE_SOURCES
    + CODEX_SOURCES
    + ZSTD_SOURCES
    + OPENCODE_SOURCES
    + GEMINI_SOURCES
    + CURSOR_SOURCES
    + CLINE_SOURCES
    + AMP_SOURCES
    + KILO_SOURCES
    + GOOSE_SOURCES
    + HERMES_SOURCES
    + OPENCLAW_SOURCES
    + DROID_SOURCES
    + KIMI_SOURCES
    + GROK_SOURCES
    + COMMANDCODE_SOURCES
    + MIMO_SOURCES
    + ZCODE_SOURCES
    + KIRO_SOURCES
    + PROMA_SOURCES
    + QODERCN_SOURCES
    + DSH_SOURCES
    + CHERRYSTUDIO_SOURCES
    + LMSTUDIO_SOURCES
    + COPILOT_SOURCES
    + CODEBUDDY_SOURCES
    + WORKBUDDY_SOURCES
    + PI_SOURCES
    + OMP_SOURCES
    + SENPI_SOURCES
    + ANTIGRAVITY_SOURCES
    + QWEN_SOURCES
    + REASONIX_SOURCES
    + MUSE_SOURCES
    + MCODE_SOURCES
    + ZED_SOURCES
    + LOCAL_USAGE_SOURCES
    + ARCHIVE_SOURCES
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


def _claude_source(
    index: int, observations: int = OBSERVATIONS_PER_JSONL
) -> tuple[str, bytes, dict[str, int], int]:
    session = f"claude-fixture-{index:03d}"
    records: list[bytes] = []
    totals = _counts()
    for observation in range(observations):
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


def _buddy_source(client: str, index: int) -> tuple[str, bytes, dict[str, int], int]:
    session = f"{client}-fixture-{index:03d}"
    totals = _counts()
    records: list[bytes] = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 300 + index * 10 + observation,
            "output": 20 + observation,
            "cacheRead": 7 + observation % 3,
            "cacheWrite": 0,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        gross_input = counts["input"] + counts["cacheRead"]
        records.append(
            _json_line(
                {
                    "id": f"{client}-assistant-{index:03d}-{observation:02d}",
                    "message": {
                        "model": "glm-5.2",
                        "usage": {
                            "cache_read_input_tokens": counts["cacheRead"],
                            "input_tokens": gross_input,
                            "output_tokens": counts["output"],
                            "total_tokens": gross_input + counts["output"],
                        },
                    },
                    "providerData": {
                        "messageId": f"{client}-message-{index:03d}-{observation:02d}",
                        "model": "glm-5.2",
                    },
                    "role": "assistant",
                    "sessionId": session,
                    "status": "completed",
                    "timestamp": 1_780_000_000_000 + observation * 1_000,
                    "type": "message",
                }
            )
        )
    return session, b"".join(records), totals, len(records)


def _pi_source(
    client: str, index: int
) -> tuple[str, bytes, dict[str, int], int]:
    session = f"{client}-fixture-{index:03d}"
    records = [
        _json_line(
            {
                "cwd": "/synthetic",
                "id": session,
                "timestamp": "2026-11-13T12:00:00Z",
                "type": "session",
                "version": 3,
            }
        )
    ]
    totals = _counts()
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 150 + index * 10 + observation,
            "output": 18 + observation,
            "cacheRead": 4 + observation % 2,
            "cacheWrite": 2,
            # Pi documents reasoning as a subset of output; the native reader
            # keeps the canonical reasoning bucket at zero.
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        records.append(
            _json_line(
                {
                    "id": f"{client}-message-{index:03d}-{observation:02d}",
                    "message": {
                        "model": "claude-sonnet-4-5",
                        "provider": "anthropic",
                        "role": "assistant",
                        "usage": {
                            "cacheRead": counts["cacheRead"],
                            "cacheWrite": counts["cacheWrite"],
                            "input": counts["input"],
                            "output": counts["output"],
                            "reasoning": 5,
                            "totalTokens": counts["input"]
                            + counts["output"]
                            + counts["cacheRead"]
                            + counts["cacheWrite"],
                        },
                    },
                    "timestamp": f"2026-11-13T12:{index % 60:02d}:{observation + 1:02d}Z",
                    "type": "message",
                }
            )
        )
    return session, b"".join(records), totals, OBSERVATIONS_PER_JSONL


def _antigravity_source(_client: str, index: int) -> tuple[str, bytes, dict[str, int], int]:
    session = f"antigravity-fixture-{index:03d}"
    records = [
        _json_line(
            {
                "type": "session_meta",
                "sessionId": session,
                "modelId": "claude-sonnet-4-6",
            }
        )
    ]
    totals = _counts()
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 100 + index * 10 + observation,
            "output": 20 + observation,
            "cacheRead": 5,
            "cacheWrite": 2,
            "reasoning": 1,
        }
        _add_counts(totals, counts)
        records.append(
            _json_line(
                {
                    "type": "usage",
                    "sessionId": session,
                    "timestamp": 1_780_000_000_000 + observation * 1_000,
                    "responseId": f"ag-{index:03d}-{observation:02d}",
                    **{key: counts[key] for key in ("input", "output", "cacheRead", "cacheWrite", "reasoning")},
                }
            )
        )
    return session, b"".join(records), totals, OBSERVATIONS_PER_JSONL


def _qwen_source(_client: str, index: int) -> tuple[str, bytes, dict[str, int], int]:
    session = f"qwen-fixture-{index:03d}"
    totals = _counts()
    records: list[bytes] = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 100 + index * 10 + observation,
            "output": 20 + observation,
            "cacheRead": 5,
            "cacheWrite": 0,
            "reasoning": 3,
        }
        _add_counts(totals, counts)
        prompt = counts["input"] + counts["cacheRead"]
        candidates = counts["output"]
        thoughts = counts["reasoning"]
        records.append(
            _json_line(
                {
                    "type": "assistant",
                    "sessionId": session,
                    "model": "qwen3-coder",
                    "timestamp": f"2026-11-13T10:{index % 60:02d}:{observation:02d}Z",
                    "usageMetadata": {
                        "promptTokenCount": prompt,
                        "candidatesTokenCount": candidates,
                        "thoughtsTokenCount": thoughts,
                        "cachedContentTokenCount": counts["cacheRead"],
                        "totalTokenCount": prompt + candidates + thoughts,
                    },
                }
            )
        )
    return session, b"".join(records), totals, OBSERVATIONS_PER_JSONL


def _reasonix_source(_client: str, index: int) -> tuple[str, bytes, dict[str, int], int]:
    session = f"reasonix-fixture-{index:03d}"
    totals = _counts()
    records: list[bytes] = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 100 + index * 10 + observation,
            "output": 20 + observation,
            "cacheRead": 5,
            "cacheWrite": 0,
            "reasoning": 3,
        }
        _add_counts(totals, counts)
        records.append(
            _json_line(
                {
                    "ts": f"2026-11-13T10:{index % 60:02d}:{observation:02d}Z",
                    "model": "deepseek/chat",
                    "prompt": counts["input"] + counts["cacheRead"],
                    "completion": counts["output"] + counts["reasoning"],
                    "reasoning": counts["reasoning"],
                    "cache_hit": counts["cacheRead"],
                    "cache_miss": counts["input"],
                    "total": counts["input"] + counts["cacheRead"] + counts["output"] + counts["reasoning"],
                    "requests": 1,
                }
            )
        )
    return session, b"".join(records), totals, OBSERVATIONS_PER_JSONL


def _muse_source(_client: str, index: int) -> tuple[str, bytes, dict[str, int], int]:
    session = f"muse-fixture-{index:03d}"
    totals = _counts()
    records: list[bytes] = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 100 + index * 10 + observation,
            "output": 20 + observation,
            "cacheRead": 5,
            "cacheWrite": 0,
            "reasoning": 3,
        }
        _add_counts(totals, counts)
        records.append(
            _json_line(
                {
                    "recorded_at": 1_780_000_000_000_000 + observation * 1_000_000,
                    "sequence": observation,
                    "stream": {"id": session},
                    "payload_type": "runtime.session",
                    "payload": {
                        "kind": "run",
                        "event": {
                            "kind": "model_completed",
                            "model": "muse-spark-1.3-contributor",
                            "usage": {
                                "input_tokens": counts["input"] + counts["cacheRead"],
                                "output_tokens": counts["output"] + counts["reasoning"],
                                "cached_tokens": counts["cacheRead"],
                                "cache_read_tokens": counts["cacheRead"],
                                "cache_write_tokens": 0,
                                "reasoning_tokens": counts["reasoning"],
                            },
                        },
                    },
                }
            )
        )
    return session, b"".join(records), totals, OBSERVATIONS_PER_JSONL


def _mcode_source(_client: str, index: int) -> tuple[str, bytes, dict[str, int], int]:
    session = f"mcode-fixture-{index:03d}"
    totals = _counts()
    records: list[bytes] = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 100 + index * 10 + observation,
            "output": 20 + observation,
            "cacheRead": 0,
            "cacheWrite": 0,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        records.append(
            _json_line(
                {
                    "type": "exec.completed",
                    "turnId": f"turn-{observation:02d}",
                    "sessionId": session,
                    "timestampMs": 1_780_000_000_000 + observation * 1_000,
                    "result": {
                        "model": {"providerId": "minimax", "modelId": "MiniMax-M2"},
                        "usage": {
                            "inputTokens": counts["input"],
                            "outputTokens": counts["output"],
                            "cacheReadTokens": 0,
                            "cacheWriteTokens": 0,
                        },
                    },
                }
            )
        )
    return session, b"".join(records), totals, OBSERVATIONS_PER_JSONL


def _opencode_database(path: Path) -> tuple[str, dict[str, int], int]:
    """Create the current OpenCode v2 shape: session_v2 + session_message."""
    session = "opencode-fixture-000"
    totals = _counts()
    created = 1_763_000_000_000

    def build(connection: sqlite3.Connection) -> None:
        connection.executescript(
            """
            CREATE TABLE session_v2 (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                directory TEXT NOT NULL,
                time_created INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                seq INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            ) STRICT;
            """
        )
        connection.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created) VALUES (?, ?, ?, ?)",
            (session, "Synthetic", "/synthetic", created),
        )
        for observation in range(OBSERVATIONS_PER_JSONL):
            counts = {
                "input": 250 + observation,
                "output": 25 + observation,
                "cacheRead": 7,
                "cacheWrite": 3,
                "reasoning": 2,
            }
            _add_counts(totals, counts)
            timestamp = created + observation * 1_000
            data = {
                "role": "assistant",
                "model": {"id": "claude-sonnet-4-5", "providerID": "anthropic"},
                "time": {"created": timestamp},
                # OpenCode stores inclusive output; the reader splits reasoning
                # out once, so expectedTotals keeps the non-reasoning portion.
                "tokens": {
                    "input": counts["input"],
                    "output": counts["output"] + counts["reasoning"],
                    "reasoning": counts["reasoning"],
                    "cache": {"read": counts["cacheRead"], "write": counts["cacheWrite"]},
                },
                "cost": 0.001,
            }
            connection.execute(
                "INSERT INTO session_message "
                "(id, session_id, type, seq, time_created, data) VALUES (?, ?, ?, ?, ?, ?)",
                (
                    f"opencode-message-{observation:02d}",
                    session,
                    "assistant",
                    observation,
                    timestamp,
                    json.dumps(data, sort_keys=True, separators=(",", ":")),
                ),
            )

    connection = sqlite3.connect(path)
    try:
        build(connection)
        connection.commit()
    finally:
        connection.close()
    return session, totals, OBSERVATIONS_PER_JSONL


def _gemini_source() -> tuple[str, bytes, dict[str, int], int]:
    session = "gemini-fixture-000"
    totals = _counts()
    messages = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 197 + observation,
            "output": 30 + observation,
            "cacheRead": 5,
            "cacheWrite": 0,
            "reasoning": 4,
        }
        _add_counts(totals, counts)
        raw_input = 200 + observation
        raw_output = 30 + observation
        messages.append(
            {
                "id": f"gemini-message-{observation:02d}",
                "timestamp": f"2026-11-13T10:00:{observation:02d}Z",
                "model": "gemini-2.5-pro",
                "tokens": {
                    "input": raw_input,
                    "output": raw_output,
                    "cached": 5,
                    "thoughts": 4,
                    "tool": 2,
                    "total": raw_input + raw_output + 4 + 2,
                },
            }
        )
    data = json.dumps(
        {"sessionId": session, "messages": messages},
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return session, data, totals, len(messages)


def _cursor_source() -> tuple[str, bytes, dict[str, int], int]:
    session = "cursor-fixture-000"
    totals = _counts()
    events = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 400 + observation,
            "output": 50 + observation,
            "cacheRead": 20,
            "cacheWrite": 5,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        events.append(
            {
                "usageEventId": f"cursor-event-{observation:02d}",
                "conversationId": session,
                "timestamp": str(1_763_000_000_000 + observation * 1_000),
                "model": "claude-sonnet-4-5",
                "tokenUsage": {
                    "inputTokens": counts["input"],
                    "outputTokens": counts["output"],
                    "cacheReadTokens": counts["cacheRead"],
                    "cacheWriteTokens": counts["cacheWrite"],
                },
            }
        )
    data = json.dumps(
        {"usageEventsDisplay": events},
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return session, data, totals, len(events)


def _cline_source() -> tuple[str, bytes, dict[str, int], int]:
    session = "cline-fixture-000"
    totals = _counts()
    messages = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        raw_input = 300 + observation
        counts = {
            "input": raw_input - 15,
            "output": 40 + observation,
            "cacheRead": 10,
            "cacheWrite": 5,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        messages.append(
            {
                "id": f"cline-message-{observation:02d}",
                "role": "assistant",
                "ts": 1_763_000_000_000 + observation * 1_000,
                "modelInfo": {"id": "claude-sonnet-4-5", "provider": "anthropic"},
                "metrics": {
                    "inputTokens": raw_input,
                    "outputTokens": counts["output"],
                    "cacheReadTokens": counts["cacheRead"],
                    "cacheWriteTokens": counts["cacheWrite"],
                },
            }
        )
    data = json.dumps(
        {"sessionId": session, "agent": "main", "messages": messages},
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return session, data, totals, len(messages)


def _amp_source() -> tuple[str, bytes, dict[str, int], int]:
    session = "amp-fixture-000"
    totals = _counts()
    events = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 400 + observation,
            "output": 50 + observation,
            "cacheRead": 20,
            "cacheWrite": 5,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        events.append(
            {
                "model": "claude-sonnet-4-5",
                "timestamp": f"2026-11-13T10:00:{observation:02d}Z",
                "tokens": {
                    "input": counts["input"],
                    "output": counts["output"],
                    "cacheReadInputTokens": counts["cacheRead"],
                    "cacheCreationInputTokens": counts["cacheWrite"],
                },
            }
        )
    data = json.dumps(
        {"id": session, "created": 1_763_000_000_000, "usageLedger": {"events": events}},
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return session, data, totals, len(events)


def _kilo_database(path: Path) -> tuple[str, dict[str, int], int]:
    session = "kilo-fixture-000"
    totals = _counts()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL) STRICT;"
        )
        for observation in range(OBSERVATIONS_PER_JSONL):
            counts = {
                "input": 500 + observation,
                "output": 55 + observation,
                "cacheRead": 15,
                "cacheWrite": 5,
                "reasoning": 5,
            }
            _add_counts(totals, counts)
            data = {
                "role": "assistant",
                "modelID": "minimax/m2.5",
                "providerID": "kilo",
                "tokens": {
                    "input": counts["input"],
                    "output": counts["output"] + counts["reasoning"],
                    "reasoning": counts["reasoning"],
                    "cache": {"read": counts["cacheRead"], "write": counts["cacheWrite"]},
                },
                "time": {"created": 1_763_000_000_000 + observation * 1_000},
            }
            connection.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?, ?, ?)",
                (
                    f"kilo-message-{observation:02d}",
                    session,
                    json.dumps(data, sort_keys=True, separators=(",", ":")),
                ),
            )
        connection.commit()
    finally:
        connection.close()
    return session, totals, OBSERVATIONS_PER_JSONL


def _zed_database(path: Path) -> tuple[str, dict[str, int], int]:
    session = "zed-fixture-000"
    totals = _counts()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            """CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                updated_at TEXT NOT NULL,
                created_at TEXT,
                data_type TEXT NOT NULL,
                data BLOB NOT NULL
            ) STRICT;"""
        )
        usages = {}
        for observation in range(OBSERVATIONS_PER_JSONL):
            counts = {
                "input": 100 + observation,
                "output": 20 + observation,
                "cacheRead": 5,
                "cacheWrite": 2,
                "reasoning": 0,
            }
            _add_counts(totals, counts)
            usages[str(observation)] = {
                "input_tokens": counts["input"],
                "output_tokens": counts["output"],
                "cache_read_input_tokens": counts["cacheRead"],
                "cache_creation_input_tokens": counts["cacheWrite"],
            }
        data = json.dumps(
            {
                "model": {"provider": "zed.dev", "model": "claude-sonnet-4-6"},
                "request_token_usage": usages,
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
        connection.execute(
            "INSERT INTO threads VALUES (?, ?, ?, 'json', ?)",
            (session, "2026-11-13T10:00:00Z", "2026-11-13T09:00:00Z", data),
        )
        connection.commit()
    finally:
        connection.close()
    return session, totals, 1


def _goose_database(path: Path) -> tuple[str, dict[str, int], int]:
    session = "goose-fixture-000"
    totals = _counts()
    totals["input"] = 5_000
    totals["output"] = 1_000
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            """CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                model_config_json TEXT,
                provider_name TEXT,
                created_at TEXT NOT NULL,
                total_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                accumulated_total_tokens INTEGER,
                accumulated_input_tokens INTEGER,
                accumulated_output_tokens INTEGER
            ) STRICT;"""
        )
        connection.execute(
            "INSERT INTO sessions VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                session,
                '{"model_name":"claude-sonnet-4-5"}',
                "anthropic",
                "2026-11-13T10:00:00Z",
                6_000,
                5_000,
                1_000,
                6_000,
                5_000,
                1_000,
            ),
        )
        connection.commit()
    finally:
        connection.close()
    return session, totals, 1


def _hermes_database(path: Path) -> tuple[str, dict[str, int], int]:
    """One Hermes session that used several models, broken down per model.

    Hermes records per-session aggregates under `session_model_usage`, so a
    single session spanning models is one canonical session with one
    observation per model/provider group.
    """
    session = "hermes-fixture-000"
    totals = _counts()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            """CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                model TEXT,
                billing_provider TEXT,
                started_at REAL NOT NULL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                reasoning_tokens INTEGER
            ) STRICT;"""
        )
        connection.execute(
            """CREATE TABLE session_model_usage (
                session_id TEXT NOT NULL,
                model TEXT,
                billing_provider TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                reasoning_tokens INTEGER,
                PRIMARY KEY (session_id, model, billing_provider)
            ) STRICT;"""
        )
        connection.execute(
            "INSERT INTO sessions VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (session, "claude-sonnet-4-5", "anthropic", 1_784_000_000.0, 0, 0, 0, 0, 0),
        )
        rows = []
        for observation in range(OBSERVATIONS_PER_JSONL):
            # Hermes stores inclusive output; the reader subtracts the reasoning
            # subset, so the declared totals carry the additive output while the
            # database row stores output + reasoning.
            counts = {
                "input": 500,
                "output": 80,
                "cacheRead": 0,
                "cacheWrite": 0,
                "reasoning": 20,
            }
            _add_counts(totals, counts)
            rows.append(
                (
                    session,
                    f"hermes-model-{observation:02d}",
                    "anthropic",
                    counts["input"],
                    counts["output"] + counts["reasoning"],
                    0,
                    0,
                    counts["reasoning"],
                )
            )
        connection.executemany(
            "INSERT INTO session_model_usage VALUES (?, ?, ?, ?, ?, ?, ?, ?)", rows
        )
        connection.commit()
    finally:
        connection.close()
    return session, totals, len(rows)


def _openclaw_database(path: Path) -> tuple[str, dict[str, int], int]:
    """One OpenClaw agent store: `transcript_events`, one event per row."""
    session = "openclaw-fixture-000"
    totals = _counts()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            """CREATE TABLE transcript_events (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, seq)
            ) STRICT;"""
        )
        rows = []
        for observation in range(OBSERVATIONS_PER_JSONL):
            counts = {
                "input": 400,
                "output": 80,
                "cacheRead": 20,
                "cacheWrite": 0,
                "reasoning": 10,
            }
            _add_counts(totals, counts)
            event = {
                "type": "message",
                "id": f"ev-{observation:03d}",
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude-sonnet-4-5",
                    "usage": {
                        "input": counts["input"],
                        # OpenClaw stores inclusive output; reasoning is a subset.
                        "output": counts["output"] + counts["reasoning"],
                        "cacheRead": counts["cacheRead"],
                        "cacheWrite": counts["cacheWrite"],
                        "reasoningTokens": counts["reasoning"],
                    },
                    "timestamp": 1_784_000_000_000 + observation,
                },
            }
            rows.append(
                (
                    session,
                    observation,
                    json.dumps(event, sort_keys=True),
                    1_784_000_000_000 + observation,
                )
            )
        connection.executemany(
            "INSERT INTO transcript_events VALUES (?, ?, ?, ?)", rows
        )
        connection.commit()
    finally:
        connection.close()
    return session, totals, len(rows)


def _droid_source() -> tuple[str, bytes, dict[str, int], int]:
    """One Droid `*.settings.json` document: a single cumulative usage block."""
    session = "droid-fixture-000"
    totals = _counts()
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 300,
            "output": 60,
            "cacheRead": 15,
            "cacheWrite": 5,
            "reasoning": 0,
        }
        _add_counts(totals, counts)

    document = {
        "model": "claude-sonnet-4-5",
        "providerLock": "anthropic",
        "tokenUsage": {
            "inputTokens": totals["input"],
            "outputTokens": totals["output"],
            "cacheCreationTokens": totals["cacheWrite"],
            "cacheReadTokens": totals["cacheRead"],
            "thinkingTokens": totals["reasoning"],
        },
    }
    data = json.dumps(document, sort_keys=True, separators=(",", ":")).encode("utf-8")
    # One document is one observation, however many replies it stands for.
    return session, data, totals, 1


def _kimi_source() -> tuple[str, bytes, dict[str, int], int]:
    """One Kimi `wire.jsonl`: a single session reporting usage per turn."""
    group = "kimi-fixture-group"
    session = "kimi-fixture-000"
    records: list[bytes] = []
    totals = _counts()
    records.append(
        _json_line({"type": "metadata", "protocol_version": "1.3"})
    )
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 320 + observation,
            "output": 40 + observation,
            "cacheRead": 12,
            "cacheWrite": 3,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        records.append(
            _json_line(
                {
                    "timestamp": 1770983426.0 + observation,
                    "message": {
                        "type": "StatusUpdate",
                        "payload": {
                            "token_usage": {
                                "input_other": counts["input"],
                                "output": counts["output"],
                                "input_cache_read": counts["cacheRead"],
                                "input_cache_creation": counts["cacheWrite"],
                            },
                            "message_id": f"chatcmpl-{observation:03d}",
                        },
                    },
                }
            )
        )
    data = b"".join(records)
    return f"{group}/{session}", data, totals, OBSERVATIONS_PER_JSONL


def _grok_source() -> tuple[str, bytes, dict[str, int], int]:
    """One Grok `updates.jsonl`: one `turn_completed` per observation."""
    workspace = "workspace-fixture"
    session = "grok-fixture-000"
    records: list[bytes] = []
    totals = _counts()
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 400 + observation,
            "output": 60 + observation,
            "cacheRead": 25,
            "cacheWrite": 0,
            "reasoning": 8,
        }
        _add_counts(totals, counts)
        # Grok's inputTokens includes cachedReadTokens, and outputTokens
        # includes reasoningTokens, so emit the inclusive wire values.
        records.append(
            _json_line(
                {
                    "params": {
                        "_meta": {"agentTimestampMs": 1770983426000 + observation},
                        "update": {
                            "sessionUpdate": "turn_completed",
                            "promptId": f"prompt-{observation:03d}",
                            "_meta": {"modelId": "grok-4"},
                            "usage": {
                                "inputTokens": counts["input"] + counts["cacheRead"],
                                "outputTokens": counts["output"] + counts["reasoning"],
                                "cachedReadTokens": counts["cacheRead"],
                                "reasoningTokens": counts["reasoning"],
                            },
                        },
                    },
                }
            )
        )
    data = b"".join(records)
    return f"{workspace}/{session}", data, totals, OBSERVATIONS_PER_JSONL


def _commandcode_source() -> tuple[str, bytes, dict[str, int], int]:
    """One Command Code v3 transcript: one assistant line per observation."""
    slug = "-home-fixture-repo"
    session = "commandcode-fixture-000"
    records: list[bytes] = [
        _json_line({"type": "session", "version": 3, "id": session})
    ]
    totals = _counts()
    parent = ""
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 180 + observation,
            "output": 45 + observation,
            "cacheRead": 20,
            "cacheWrite": 4,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        entry_id = f"entry-{observation:03d}"
        # Command Code's buckets are disjoint: `inputTokens` is cache-exclusive
        # and the cache buckets are billed on top, so the wire values are the
        # recorded counts unchanged.
        records.append(
            _json_line(
                {
                    "type": "message",
                    "id": entry_id,
                    "parentId": parent,
                    "timestamp": f"2026-02-13T10:{30 + observation // 60:02d}:"
                    f"{observation % 60:02d}.420Z",
                    "model": "deepseek/deepseek-v4-flash",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": "ok"}],
                    },
                    "usage": {
                        "inputTokens": counts["input"],
                        "outputTokens": counts["output"],
                        "cacheReadTokens": counts["cacheRead"],
                        "cacheWriteTokens": counts["cacheWrite"],
                        "costUsd": 0.000123,
                    },
                }
            )
        )
        parent = entry_id
    data = b"".join(records)
    return f"{slug}/{session}", data, totals, OBSERVATIONS_PER_JSONL


def _proma_source() -> tuple[str, bytes, dict[str, int], int]:
    """One Proma legacy SDK transcript: one assistant line per observation."""
    session = "proma-fixture-000"
    totals = _counts()
    lines = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 170 + observation,
            "output": 34 + observation,
            "cacheRead": 9,
            "cacheWrite": 2,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        lines.append(
            {
                "type": "assistant",
                "session_id": session,
                "_createdAt": f"2026-06-20T10:00:{observation:02d}.000Z",
                "message": {
                    "id": f"resp-{observation:03d}",
                    "model": "claude-sonnet-4",
                    "provider": "proma",
                    "usage": {
                        "input_tokens": counts["input"],
                        "output_tokens": counts["output"],
                        "cache_read_input_tokens": counts["cacheRead"],
                        "cache_creation_input_tokens": counts["cacheWrite"],
                    },
                },
            }
        )
    data = b"".join(_json_line(entry) for entry in lines)
    return f"{session}.jsonl", data, totals, len(lines)


def _cherrystudio_database(path: Path) -> tuple[str, dict[str, int], int]:
    """One Cherry Studio chat ledger, one invocation row per observation.

    The ledger's counters are inclusive: `input_tokens` is the whole prompt and
    `output_tokens` the whole completion, with the cache and reasoning shares
    counted again in their own columns. The inclusive figures are stored here
    and the native reader splits them back out.
    """
    session = "cherrystudio-fixture-000"
    totals = _counts()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.executescript(
            """
            CREATE TABLE ai_usage_record (
                id TEXT PRIMARY KEY,
                message_id TEXT,
                provider_id TEXT,
                model_id TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                reasoning_tokens INTEGER,
                cost REAL,
                cost_currency TEXT,
                created_at INTEGER,
                message_kind TEXT,
                record_kind TEXT
            );
            CREATE TABLE message (id TEXT PRIMARY KEY, topic_id TEXT);
            CREATE TABLE topic (id TEXT PRIMARY KEY, name TEXT);
            """
        )
        for observation in range(OBSERVATIONS_PER_JSONL):
            counts = {
                "input": 190 + observation,
                "output": 38 + observation,
                "cacheRead": 8,
                "cacheWrite": 3,
                "reasoning": 4,
            }
            _add_counts(totals, counts)
            connection.execute(
                "INSERT INTO ai_usage_record"
                " (id, message_id, provider_id, model_id, input_tokens, output_tokens,"
                "  cache_read_tokens, cache_write_tokens, reasoning_tokens, cost,"
                "  cost_currency, created_at, message_kind, record_kind)"
                " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, 'chat', 'invocation')",
                (
                    f"row-{observation:03d}",
                    f"msg-{observation:03d}",
                    "anthropic",
                    "claude-sonnet-4",
                    counts["input"] + counts["cacheRead"] + counts["cacheWrite"],
                    counts["output"] + counts["reasoning"],
                    counts["cacheRead"],
                    counts["cacheWrite"],
                    counts["reasoning"],
                    1_782_718_000_000 + observation * 1000,
                ),
            )
        connection.commit()
    finally:
        connection.close()
    return session, totals, OBSERVATIONS_PER_JSONL


def _lmstudio_source() -> tuple[str, bytes, dict[str, int], int]:
    """One LM Studio server log: one pretty-printed response per observation.

    The usage object deliberately spans several lines, which is how the server
    writes it and why the reader scans for balanced objects instead of parsing
    line-delimited JSON.
    """
    session = "llama.log"
    totals = _counts()
    blocks = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 170 + observation,
            "output": 34 + observation,
            "cacheRead": 6,
            "cacheWrite": 2,
            "reasoning": 3,
        }
        _add_counts(totals, counts)
        prompt = counts["input"] + counts["cacheRead"] + counts["cacheWrite"]
        completion = counts["output"] + counts["reasoning"]
        blocks.append(
            "[2026-07-09 11:00:%02d][INFO][llama-3]\n"
            "Final response: {\n"
            '  "id": "chatcmpl-%03d",\n'
            '  "model": "llama-3",\n'
            '  "usage": {\n'
            '    "prompt_tokens": %d,\n'
            '    "completion_tokens": %d,\n'
            '    "total_tokens": %d,\n'
            '    "prompt_tokens_details": {\n'
            '      "cached_tokens": %d,\n'
            '      "cache_creation_input_tokens": %d\n'
            "    },\n"
            '    "output_tokens_details": {\n'
            '      "reasoning_tokens": %d\n'
            "    }\n"
            "  }\n"
            "}\n"
            % (
                observation % 60,
                observation,
                prompt,
                completion,
                prompt + completion,
                counts["cacheRead"],
                counts["cacheWrite"],
                counts["reasoning"],
            )
        )
    return session, "".join(blocks).encode(), totals, OBSERVATIONS_PER_JSONL


def _dsh_source() -> tuple[str, bytes, dict[str, int], int]:
    """One DSH session: a header plus one settlement per observation."""
    session = "dsh-fixture-000"
    totals = _counts()
    lines = [
        {
            "type": "session",
            "id": session,
            "cwd": "/work/dsh-fixture",
            "seq": 0,
        }
    ]
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 190 + observation,
            "output": 38 + observation,
            "cacheRead": 8,
            "cacheWrite": 3,
            "reasoning": 4,
        }
        _add_counts(totals, counts)
        lines.append(
            {
                "type": "assistant/message",
                "time": 1_782_718_000_000 + observation * 1000,
                "seq": observation + 1,
                "data": {
                    "turn": 0,
                    "step": observation,
                    "message": {
                        "id": f"msg-{observation:03d}",
                        "source": {
                            "provider": "deepseek",
                            "model": "deepseek-chat",
                        },
                    },
                    "usage": {
                        "inputTokens": counts["input"],
                        # outputTokens includes reasoningTokens.
                        "outputTokens": counts["output"] + counts["reasoning"],
                        "cacheReadTokens": counts["cacheRead"],
                        "cacheWriteTokens": counts["cacheWrite"],
                        "reasoningTokens": counts["reasoning"],
                    },
                },
            }
        )
    data = b"".join(_json_line(entry) for entry in lines)
    return "session.jsonl", data, totals, len(lines) - 1


def _qodercn_source() -> tuple[str, bytes, dict[str, int], int]:
    """One Qoder CN transcript: one assistant line per observation.

    Qoder CN reports a cache-inclusive prompt count, so the recorded
    `input_tokens` is the net input plus the cache read, and the native reader
    subtracts the overlap back out.
    """
    session = "qodercn-fixture-000"
    totals = _counts()
    lines = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 180 + observation,
            "output": 36 + observation,
            "cacheRead": 11,
            "cacheWrite": 0,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        lines.append(
            {
                "type": "assistant",
                "sessionId": session,
                "timestamp": f"2026-06-20T10:00:{observation:02d}.000Z",
                "message": {
                    "id": f"msg-{observation:03d}",
                    "model": "qmodel",
                    "usage": {
                        "input_tokens": counts["input"] + counts["cacheRead"],
                        "cache_read_input_tokens": counts["cacheRead"],
                        "output_tokens": counts["output"],
                    },
                },
            }
        )
    data = b"".join(_json_line(entry) for entry in lines)
    return f"{session}.jsonl", data, totals, len(lines)


def _kiro_source() -> tuple[str, bytes, dict[str, int], int]:
    """One Kiro CLI session: one authoritative turn per observation."""
    session = "kiro-fixture-000"
    totals = _counts()
    turns = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 210 + observation,
            "output": 40 + observation,
            "cacheRead": 12,
            "cacheWrite": 2,
            "reasoning": 0,
        }
        _add_counts(totals, counts)
        turns.append(
            {
                "input_token_count": counts["input"],
                "output_token_count": counts["output"],
                "cache_read_input_token_count": counts["cacheRead"],
                "cache_write_input_token_count": counts["cacheWrite"],
                "end_timestamp": 1782718000 + observation,
            }
        )
    document = {
        "session_id": session,
        "cwd": "/work/acceptance",
        "session_state": {
            "rts_model_state": {
                "model_info": {
                    "model_id": "claude-sonnet-4",
                    "context_window_tokens": 200_000,
                }
            },
            "conversation_metadata": {"user_turn_metadatas": turns},
        },
    }
    return (
        f"{session}.json",
        json.dumps(document, sort_keys=True, separators=(",", ":")).encode("utf-8"),
        totals,
        len(turns),
    )


def _zcode_database(path: Path) -> tuple[str, dict[str, int], int]:
    """One ZCode `model_usage` store: cache-inclusive rows with a reported total."""
    session = "zcode-fixture-000"
    totals = _counts()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.executescript(
            """
            CREATE TABLE model_usage (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                turn_id TEXT,
                model_id TEXT,
                started_at INTEGER,
                completed_at INTEGER,
                duration_ms INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                reasoning_tokens INTEGER,
                cache_read_input_tokens INTEGER,
                cache_creation_input_tokens INTEGER,
                computed_total_tokens INTEGER,
                agent TEXT,
                mode TEXT
            );
            CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, path TEXT);
            """
        )
        for observation in range(OBSERVATIONS_PER_JSONL):
            counts = {
                "input": 140 + observation,
                "output": 45 + observation,
                "cacheRead": 20,
                "cacheWrite": 4,
                "reasoning": 6,
            }
            _add_counts(totals, counts)
            # ZCode reports input/output cache- and reasoning-inclusive, and
            # `computed_total_tokens` equals input+output. That is what proves
            # the inclusive shape to the reader, which then subtracts the
            # overlap so the canonical total stays additive. The expected totals
            # above therefore hold the *net* buckets, while the row stores the
            # inclusive ones.
            raw_input = counts["input"] + counts["cacheRead"] + counts["cacheWrite"]
            raw_output = counts["output"] + counts["reasoning"]
            connection.execute(
                """
                INSERT INTO model_usage (
                    id, session_id, turn_id, model_id, started_at, completed_at,
                    duration_ms, input_tokens, output_tokens, reasoning_tokens,
                    cache_read_input_tokens, cache_creation_input_tokens,
                    computed_total_tokens, agent, mode
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    f"usage-{observation:03d}",
                    session,
                    f"turn-{observation:03d}",
                    "glm-5.2",
                    1782718000000 + observation * 1000,
                    1782718000500 + observation * 1000,
                    500,
                    raw_input,
                    raw_output,
                    counts["reasoning"],
                    counts["cacheRead"],
                    counts["cacheWrite"],
                    raw_input + raw_output,
                    "zcode-agent",
                    "yolo",
                ),
            )
        connection.commit()
    finally:
        connection.close()
    return session, totals, OBSERVATIONS_PER_JSONL


def _mimo_database(path: Path) -> tuple[str, dict[str, int], int]:
    """One MiMo database: assistant rows in OpenCode's `message` schema."""
    session = "mimo-fixture-000"
    totals = _counts()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL) STRICT;"
        )
        for observation in range(OBSERVATIONS_PER_JSONL):
            counts = {
                "input": 130 + observation,
                "output": 35 + observation,
                "cacheRead": 15,
                "cacheWrite": 3,
                "reasoning": 0,
            }
            _add_counts(totals, counts)
            data = {
                "role": "assistant",
                "modelID": "mimo-v2",
                "providerID": "xiaomi",
                "tokens": {
                    "input": counts["input"],
                    "output": counts["output"],
                    "cache": {
                        "read": counts["cacheRead"],
                        "write": counts["cacheWrite"],
                    },
                },
                "time": {"created": 1770983426000 + observation},
            }
            connection.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?, ?, ?)",
                (
                    f"msg-{observation:03d}",
                    session,
                    json.dumps(data, sort_keys=True, separators=(",", ":")),
                ),
            )
        connection.commit()
    finally:
        connection.close()
    return session, totals, OBSERVATIONS_PER_JSONL


def _copilot_source() -> tuple[str, bytes, dict[str, int], int]:
    session = "copilot-fixture-000"
    totals = _counts()
    lines = []
    for observation in range(OBSERVATIONS_PER_JSONL):
        counts = {
            "input": 590 + observation,
            "output": 50 + observation,
            "cacheRead": 10,
            "cacheWrite": 0,
            "reasoning": 4,
        }
        _add_counts(totals, counts)
        lines.append(
            json.dumps(
                {
                    "type": "span",
                    "traceId": f"trace-{observation:03d}",
                    "spanId": f"span-{observation:03d}",
                    "name": "chat gpt-5",
                    "startTime": f"2026-11-13T10:00:{observation:02d}Z",
                    "attributes": {
                        "gen_ai.operation.name": "chat",
                        "gen_ai.response.model": "gpt-5",
                        "gen_ai.conversation.id": session,
                        "gen_ai.response.id": f"response-{observation:03d}",
                        "gen_ai.usage.input_tokens": counts["input"] + counts["cacheRead"],
                        "gen_ai.usage.output_tokens": counts["output"],
                        "gen_ai.usage.cache_read.input_tokens": counts["cacheRead"],
                        "gen_ai.usage.reasoning.output_tokens": counts["reasoning"],
                    },
                },
                sort_keys=True,
                separators=(",", ":"),
            )
        )
    data = ("\n".join(lines) + "\n").encode("utf-8")
    return session, data, totals, len(lines)


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
        observations = OBSERVATIONS_PER_JSONL + (
            EXTRA_CLAUDE_OBSERVATIONS if index == 0 else 0
        )
        session, data, totals, rows = _claude_source(index, observations)
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

    for client, count, relative_root, generator in [
        ("codebuddy", CODEBUDDY_SOURCES, "home/.codebuddy/projects/acceptance", _buddy_source),
        ("workbuddy", WORKBUDDY_SOURCES, "home/.workbuddy/projects/acceptance", _buddy_source),
        ("pi", PI_SOURCES, "home/.pi/agent/sessions/acceptance", _pi_source),
        ("omp", OMP_SOURCES, "home/.omp/agent/sessions/acceptance", _pi_source),
        ("senpi", SENPI_SOURCES, "home/.senpi/agent/sessions/acceptance", _pi_source),
        ("antigravity", ANTIGRAVITY_SOURCES, "home/.config/tokscale/antigravity-cache", _antigravity_source),
        ("qwen", QWEN_SOURCES, "home/.qwen/projects/acceptance/chats", _qwen_source),
        ("reasonix", REASONIX_SOURCES, "home/.reasonix/stats", _reasonix_source),
        ("muse", MUSE_SOURCES, "home/.local/share/muse/sessions/2026/11/13", _muse_source),
        ("mcode", MCODE_SOURCES, "home/.minimax/v2/sessions/2026/11/13", _mcode_source),
    ]:
        for index in range(count):
            session, data, totals, rows = generator(client, index)
            relative = f"{relative_root}/{session}.jsonl"
            sources.append(
                _source_entry(
                    root,
                    relative,
                    data,
                    format_name=f"{client}-jsonl",
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

    opencode_target = (
        root / "home" / ".local" / "share" / "opencode" / "opencode.db"
    )
    opencode_target.parent.mkdir(parents=True, exist_ok=True)
    opencode_session, opencode_totals, opencode_rows = _opencode_database(opencode_target)
    opencode_entry = dict(
        _existing_entry(
            root, "home/.local/share/opencode/opencode.db", opencode_target
        ),
        format="opencode-sqlite",
        session=opencode_session,
        observations=opencode_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=opencode_target.stat().st_size,
        physicalBytes=opencode_target.stat().st_size,
        expectedTotals=opencode_totals,
        retainedPeriods=0,
    )
    sources.append(opencode_entry)
    _add_counts(aggregate, opencode_totals)
    _add_counts(canonical_aggregate, opencode_totals)
    physical_bytes += opencode_target.stat().st_size
    decoded_bytes += opencode_target.stat().st_size

    zed_target = (
        root / "home" / ".local" / "share" / "zed" / "threads" / "threads.db"
    )
    zed_target.parent.mkdir(parents=True, exist_ok=True)
    zed_session, zed_totals, zed_rows = _zed_database(zed_target)
    zed_entry = dict(
        _existing_entry(
            root, "home/.local/share/zed/threads/threads.db", zed_target
        ),
        format="zed-sqlite",
        session=zed_session,
        observations=zed_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=zed_target.stat().st_size,
        physicalBytes=zed_target.stat().st_size,
        expectedTotals=zed_totals,
        retainedPeriods=0,
    )
    sources.append(zed_entry)
    _add_counts(aggregate, zed_totals)
    _add_counts(canonical_aggregate, zed_totals)
    physical_bytes += zed_target.stat().st_size
    decoded_bytes += zed_target.stat().st_size

    gemini_session, gemini_data, gemini_totals, gemini_rows = _gemini_source()
    gemini_entry = _source_entry(
        root,
        "home/.gemini/tmp/fixture/chats/session-gemini-fixture-000.json",
        gemini_data,
        format_name="gemini-transcript",
        session=gemini_session,
        observations=gemini_rows,
        canonical_sessions=1,
        expected_totals=gemini_totals,
        physical_bytes=len(gemini_data),
    )
    sources.append(gemini_entry)
    _add_counts(aggregate, gemini_totals)
    _add_counts(canonical_aggregate, gemini_totals)
    physical_bytes += len(gemini_data)
    decoded_bytes += len(gemini_data)

    cursor_session, cursor_data, cursor_totals, cursor_rows = _cursor_source()
    cursor_entry = _source_entry(
        root,
        "home/.config/tokscale/cursor-cache/usage.json",
        cursor_data,
        format_name="cursor-cache-json",
        session=cursor_session,
        observations=cursor_rows,
        canonical_sessions=1,
        expected_totals=cursor_totals,
        physical_bytes=len(cursor_data),
    )
    sources.append(cursor_entry)
    _add_counts(aggregate, cursor_totals)
    _add_counts(canonical_aggregate, cursor_totals)
    physical_bytes += len(cursor_data)
    decoded_bytes += len(cursor_data)

    cline_session, cline_data, cline_totals, cline_rows = _cline_source()
    cline_entry = _source_entry(
        root,
        f"home/.cline/data/sessions/{cline_session}/{cline_session}.messages.json",
        cline_data,
        format_name="cline-json",
        session=cline_session,
        observations=cline_rows,
        canonical_sessions=1,
        expected_totals=cline_totals,
        physical_bytes=len(cline_data),
    )
    sources.append(cline_entry)
    _add_counts(aggregate, cline_totals)
    _add_counts(canonical_aggregate, cline_totals)
    physical_bytes += len(cline_data)
    decoded_bytes += len(cline_data)

    amp_session, amp_data, amp_totals, amp_rows = _amp_source()
    amp_entry = _source_entry(
        root,
        f"home/.local/share/amp/threads/{amp_session}.json",
        amp_data,
        format_name="amp-json",
        session=amp_session,
        observations=amp_rows,
        canonical_sessions=1,
        expected_totals=amp_totals,
        physical_bytes=len(amp_data),
    )
    sources.append(amp_entry)
    _add_counts(aggregate, amp_totals)
    _add_counts(canonical_aggregate, amp_totals)
    physical_bytes += len(amp_data)
    decoded_bytes += len(amp_data)

    kilo_target = root / "home" / ".local" / "share" / "kilo" / "kilo.db"
    kilo_session, kilo_totals, kilo_rows = _kilo_database(kilo_target)
    kilo_entry = dict(
        _existing_entry(root, "home/.local/share/kilo/kilo.db", kilo_target),
        format="kilo-sqlite",
        session=kilo_session,
        observations=kilo_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=kilo_target.stat().st_size,
        physicalBytes=kilo_target.stat().st_size,
        expectedTotals=kilo_totals,
        retainedPeriods=0,
    )
    sources.append(kilo_entry)
    _add_counts(aggregate, kilo_totals)
    _add_counts(canonical_aggregate, kilo_totals)
    physical_bytes += kilo_target.stat().st_size
    decoded_bytes += kilo_target.stat().st_size

    goose_target = (
        root / "home" / ".local" / "share" / "goose" / "sessions" / "sessions.db"
    )
    goose_session, goose_totals, goose_rows = _goose_database(goose_target)
    goose_entry = dict(
        _existing_entry(
            root, "home/.local/share/goose/sessions/sessions.db", goose_target
        ),
        format="goose-sqlite",
        session=goose_session,
        observations=goose_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=goose_target.stat().st_size,
        physicalBytes=goose_target.stat().st_size,
        expectedTotals=goose_totals,
        retainedPeriods=0,
    )
    sources.append(goose_entry)
    _add_counts(aggregate, goose_totals)
    _add_counts(canonical_aggregate, goose_totals)
    physical_bytes += goose_target.stat().st_size
    decoded_bytes += goose_target.stat().st_size

    hermes_target = root / "home" / ".hermes" / "state.db"
    hermes_session, hermes_totals, hermes_rows = _hermes_database(hermes_target)
    hermes_entry = dict(
        _existing_entry(root, "home/.hermes/state.db", hermes_target),
        format="hermes-sqlite",
        session=hermes_session,
        observations=hermes_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=hermes_target.stat().st_size,
        physicalBytes=hermes_target.stat().st_size,
        expectedTotals=hermes_totals,
        retainedPeriods=0,
    )
    sources.append(hermes_entry)
    _add_counts(aggregate, hermes_totals)
    _add_counts(canonical_aggregate, hermes_totals)
    physical_bytes += hermes_target.stat().st_size
    decoded_bytes += hermes_target.stat().st_size

    openclaw_target = (
        root / "home" / ".openclaw" / "agents" / "main" / "agent" / "openclaw-agent.sqlite"
    )
    openclaw_session, openclaw_totals, openclaw_rows = _openclaw_database(openclaw_target)
    openclaw_entry = dict(
        _existing_entry(
            root,
            "home/.openclaw/agents/main/agent/openclaw-agent.sqlite",
            openclaw_target,
        ),
        format="openclaw-sqlite",
        session=openclaw_session,
        observations=openclaw_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=openclaw_target.stat().st_size,
        physicalBytes=openclaw_target.stat().st_size,
        expectedTotals=openclaw_totals,
        retainedPeriods=0,
    )
    sources.append(openclaw_entry)
    _add_counts(aggregate, openclaw_totals)
    _add_counts(canonical_aggregate, openclaw_totals)
    physical_bytes += openclaw_target.stat().st_size
    decoded_bytes += openclaw_target.stat().st_size

    droid_session, droid_data, droid_totals, droid_rows = _droid_source()
    droid_entry = _source_entry(
        root,
        f"home/.factory/sessions/{droid_session}.settings.json",
        droid_data,
        format_name="droid-json",
        session=droid_session,
        observations=droid_rows,
        canonical_sessions=1,
        expected_totals=droid_totals,
        physical_bytes=len(droid_data),
    )
    sources.append(droid_entry)
    _add_counts(aggregate, droid_totals)
    _add_counts(canonical_aggregate, droid_totals)
    physical_bytes += len(droid_data)
    decoded_bytes += len(droid_data)

    kimi_path, kimi_data, kimi_totals, kimi_rows = _kimi_source()
    kimi_entry = _source_entry(
        root,
        f"home/.kimi/sessions/{kimi_path}/wire.jsonl",
        kimi_data,
        format_name="kimi-jsonl",
        session=kimi_path.rsplit("/", 1)[-1],
        observations=kimi_rows,
        canonical_sessions=1,
        expected_totals=kimi_totals,
        physical_bytes=len(kimi_data),
    )
    sources.append(kimi_entry)
    _add_counts(aggregate, kimi_totals)
    _add_counts(canonical_aggregate, kimi_totals)
    physical_bytes += len(kimi_data)
    decoded_bytes += len(kimi_data)

    grok_path, grok_data, grok_totals, grok_rows = _grok_source()
    grok_entry = _source_entry(
        root,
        f"home/.grok/sessions/{grok_path}/updates.jsonl",
        grok_data,
        format_name="grok-jsonl",
        session=grok_path.rsplit("/", 1)[-1],
        observations=grok_rows,
        canonical_sessions=1,
        expected_totals=grok_totals,
        physical_bytes=len(grok_data),
    )
    sources.append(grok_entry)
    _add_counts(aggregate, grok_totals)
    _add_counts(canonical_aggregate, grok_totals)
    physical_bytes += len(grok_data)
    decoded_bytes += len(grok_data)

    commandcode_path, commandcode_data, commandcode_totals, commandcode_rows = (
        _commandcode_source()
    )
    commandcode_entry = _source_entry(
        root,
        f"home/.commandcode/projects/{commandcode_path}.jsonl",
        commandcode_data,
        format_name="commandcode-jsonl",
        session=commandcode_path.rsplit("/", 1)[-1],
        observations=commandcode_rows,
        canonical_sessions=1,
        expected_totals=commandcode_totals,
        physical_bytes=len(commandcode_data),
    )
    sources.append(commandcode_entry)
    _add_counts(aggregate, commandcode_totals)
    _add_counts(canonical_aggregate, commandcode_totals)
    physical_bytes += len(commandcode_data)
    decoded_bytes += len(commandcode_data)

    mimo_target = root / "home" / ".local" / "share" / "mimocode" / "mimocode.db"
    mimo_session, mimo_totals, mimo_rows = _mimo_database(mimo_target)
    mimo_entry = dict(
        _existing_entry(root, "home/.local/share/mimocode/mimocode.db", mimo_target),
        format="mimo-sqlite",
        session=mimo_session,
        observations=mimo_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=mimo_target.stat().st_size,
        physicalBytes=mimo_target.stat().st_size,
        expectedTotals=mimo_totals,
        retainedPeriods=0,
    )
    sources.append(mimo_entry)
    _add_counts(aggregate, mimo_totals)
    _add_counts(canonical_aggregate, mimo_totals)
    physical_bytes += mimo_target.stat().st_size
    decoded_bytes += mimo_target.stat().st_size

    zcode_target = root / "home" / ".zcode" / "cli" / "db" / "db.sqlite"
    zcode_totals, zcode_rows = _zcode_database(zcode_target)[1:]
    zcode_entry = dict(
        _existing_entry(root, "home/.zcode/cli/db/db.sqlite", zcode_target),
        format="zcode-sqlite",
        session="db",
        observations=zcode_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=zcode_target.stat().st_size,
        physicalBytes=zcode_target.stat().st_size,
        expectedTotals=zcode_totals,
        retainedPeriods=0,
    )
    sources.append(zcode_entry)
    _add_counts(aggregate, zcode_totals)
    _add_counts(canonical_aggregate, zcode_totals)
    physical_bytes += zcode_target.stat().st_size
    decoded_bytes += zcode_target.stat().st_size

    kiro_path, kiro_data, kiro_totals, kiro_rows = _kiro_source()
    kiro_entry = _source_entry(
        root,
        f"home/.kiro/sessions/cli/{kiro_path}",
        kiro_data,
        format_name="kiro-json",
        session="kiro-fixture-000",
        observations=kiro_rows,
        canonical_sessions=1,
        expected_totals=kiro_totals,
        physical_bytes=len(kiro_data),
    )
    sources.append(kiro_entry)
    _add_counts(aggregate, kiro_totals)
    _add_counts(canonical_aggregate, kiro_totals)
    physical_bytes += len(kiro_data)
    decoded_bytes += len(kiro_data)

    proma_path, proma_data, proma_totals, proma_rows = _proma_source()
    proma_entry = _source_entry(
        root,
        f"home/.proma/sdk-config/projects/{proma_path}",
        proma_data,
        format_name="proma-jsonl",
        session="proma-fixture-000",
        observations=proma_rows,
        canonical_sessions=1,
        expected_totals=proma_totals,
        physical_bytes=len(proma_data),
    )
    sources.append(proma_entry)
    _add_counts(aggregate, proma_totals)
    _add_counts(canonical_aggregate, proma_totals)
    physical_bytes += len(proma_data)
    decoded_bytes += len(proma_data)

    qodercn_path, qodercn_data, qodercn_totals, qodercn_rows = _qodercn_source()
    qodercn_entry = _source_entry(
        root,
        f"home/.qoder-cn/projects/demo/{qodercn_path}",
        qodercn_data,
        format_name="qodercn-jsonl",
        session="qodercn-fixture-000",
        observations=qodercn_rows,
        canonical_sessions=1,
        expected_totals=qodercn_totals,
        physical_bytes=len(qodercn_data),
    )
    sources.append(qodercn_entry)
    _add_counts(aggregate, qodercn_totals)
    _add_counts(canonical_aggregate, qodercn_totals)
    physical_bytes += len(qodercn_data)
    decoded_bytes += len(qodercn_data)

    dsh_path, dsh_data, dsh_totals, dsh_rows = _dsh_source()
    dsh_entry = _source_entry(
        root,
        f"home/.dsh/sessions/work-dsh/dsh-fixture-000/{dsh_path}",
        dsh_data,
        format_name="dsh-jsonl",
        session="dsh-fixture-000",
        observations=dsh_rows,
        canonical_sessions=1,
        expected_totals=dsh_totals,
        physical_bytes=len(dsh_data),
    )
    sources.append(dsh_entry)
    _add_counts(aggregate, dsh_totals)
    _add_counts(canonical_aggregate, dsh_totals)
    physical_bytes += len(dsh_data)
    decoded_bytes += len(dsh_data)

    cherrystudio_target = (
        root / "home" / ".config" / "CherryStudio" / "Data" / "cherrystudio.sqlite"
    )
    cherrystudio_session, cherrystudio_totals, cherrystudio_rows = _cherrystudio_database(
        cherrystudio_target
    )
    cherrystudio_entry = dict(
        _existing_entry(
            root, "home/.config/CherryStudio/Data/cherrystudio.sqlite", cherrystudio_target
        ),
        format="cherrystudio-sqlite",
        session=cherrystudio_session,
        observations=cherrystudio_rows,
        canonicalSessions=1,
        suppressedObservations=0,
        decodedBytes=cherrystudio_target.stat().st_size,
        physicalBytes=cherrystudio_target.stat().st_size,
        expectedTotals=cherrystudio_totals,
        retainedPeriods=0,
    )
    sources.append(cherrystudio_entry)
    _add_counts(aggregate, cherrystudio_totals)
    _add_counts(canonical_aggregate, cherrystudio_totals)
    physical_bytes += cherrystudio_target.stat().st_size
    decoded_bytes += cherrystudio_target.stat().st_size

    lmstudio_path, lmstudio_data, lmstudio_totals, lmstudio_rows = _lmstudio_source()
    lmstudio_entry = _source_entry(
        root,
        f"home/.lmstudio/server-logs/2026-07/{lmstudio_path}",
        lmstudio_data,
        format_name="lmstudio-log",
        session="llama",
        observations=lmstudio_rows,
        canonical_sessions=1,
        expected_totals=lmstudio_totals,
        physical_bytes=len(lmstudio_data),
    )
    sources.append(lmstudio_entry)
    _add_counts(aggregate, lmstudio_totals)
    _add_counts(canonical_aggregate, lmstudio_totals)
    physical_bytes += len(lmstudio_data)
    decoded_bytes += len(lmstudio_data)

    copilot_session, copilot_data, copilot_totals, copilot_rows = _copilot_source()
    copilot_entry = _source_entry(
        root,
        f"home/.copilot/otel/{copilot_session}.jsonl",
        copilot_data,
        format_name="copilot-jsonl",
        session=copilot_session,
        observations=copilot_rows,
        canonical_sessions=1,
        expected_totals=copilot_totals,
        physical_bytes=len(copilot_data),
    )
    sources.append(copilot_entry)
    _add_counts(aggregate, copilot_totals)
    _add_counts(canonical_aggregate, copilot_totals)
    physical_bytes += len(copilot_data)
    decoded_bytes += len(copilot_data)

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
