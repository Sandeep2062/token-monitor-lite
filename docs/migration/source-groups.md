# Source groups: a source that owns more than one file

Status: **implemented for `jcode`.** `MonitoredSource::companions`, `GroupedInput`
and the keyed merge in `JcodeReader` are in place and tested. Everything else this
document once planned for turned out **not** to need it: `roocode` and the VS Code
`globalStorage` lanes are single-file readers, and Prime Agent's fork reversal is
unnecessary because the aggregate it reverses is never emitted here. See the
corrections below and the last section.

## Correction: Roo Code is single-file

This document originally claimed Roo Code needed two files per task, with
`api_conversation_history.json` carrying "the usage-bearing request history". **That was
wrong.** Roo's `api_conversation_history.json` is an Anthropic `MessageParam[]` and
contains no token counters whatsoever; all usage lives in `ui_messages.json`, in the
`say: "api_req_started"` messages. Roo Code is therefore an ordinary single-file reader
and is implemented that way (`src/parsers/roocode.rs`). Source groups is what `jcode`
needed, not what Roo Code needed.

## Why the current contract cannot express it

Every reader in this codebase is a **pure byte slice**: `ResponseReader::ingest` takes
one `&[u8]` and accumulates. `SourceInput::read_batch` yields records from exactly one
file, and the harness creates a fresh `ResponseReader` whenever a batch reports
`reset: true`. One source therefore means one file, one `source_id`, one ledger row.

Three clients need two files to describe one session:

| client | primary | companion | what the second file carries |
| --- | --- | --- | --- |
| `jcode` | `~/.jcode/sessions/session_*.json` | sibling `*.journal.jsonl` | the unsaved message suffix, plus a per-append-batch billing provider |
| `prime-agent` | parent transcript | child transcript | `child_usage_attributed` on the parent must be reversed against the child |

Reading them as two sources is wrong, not merely incomplete: the same message would be
counted twice, under two identities.

## Design

### 1. Companions on the source model

`MonitoredSource` gains `companions: Vec<PathBuf>`:

```rust
pub struct MonitoredSource {
    pub source_id: SourceId,
    pub path: PathBuf,
    pub format: SourceFormat,
    pub session: String,
    /// Read after `path`, into the same reader, in order.
    pub companions: Vec<PathBuf>,
}
```

`DiscoveredSource` does **not** need the field. `monitored_source()` is already the
boundary between discovery and monitoring, so it computes companions there. For `jcode`
the companion is pure path arithmetic (`session_abc.json` → `session_abc.journal.jsonl`),
so this needs no directory scan and no I/O.

Keeping the field off `DiscoveredSource` is deliberate: it leaves all ~50 existing
`DiscoveredSource { … }` literals in `discovery.rs` untouched, so the change stays
reviewable instead of being a 50-site mechanical diff.

### 2. A grouped input

New variant in `source_inputs.rs`:

```rust
pub struct GroupedInput {
    parts: Vec<DocumentInput>,
    /// Index of the next constituent to read. Not the count read so far.
    next: usize,
}
```

`SourceInput::Grouped(GroupedInput)`, built from `from_monitored_source` when
`companions` is non-empty. Adding a variant is a compile-error-driven change: the
exhaustive matches over `SourceInput` (`reset`, `is_current`, `read_batch`,
`adapter_status`, `read_sqlite`) each need an arm, and the compiler names every one.

The three behaviours that matter, each of which is a trap:

**`read_batch` — only the first constituent resets the generation.** `DocumentCursor`
returns `reset: true` on a fresh read, and the harness replaces the reader when it sees
that. So parts after the first must have `reset` forced to `false`, or the journal would
discard the snapshot it is supposed to amend.

**`read_batch` — every non-final constituent reports `more_available: true`.** When
`more_available` is false the harness falls through to `candidate.finish()` and
materializes the source. If part 0 reported false, part 1 would never be read. The final
constituent passes its own value through, which is what lets the source complete.

**`is_current` — compare only the parts already consumed:**

```rust
fn is_current(&self) -> io::Result<bool> {
    for part in &self.parts[..self.next] {
        if !part.is_current()? {
            return Ok(false);
        }
    }
    Ok(true)
}
```

This looks like a shortcut and is load-bearing. `read_step` calls `is_current()` at the
top of every step, and a `false` there triggers `request_full_reconciliation()`. A
naive "all parts current" check is false between part 0 and part 1 — later parts are
not stamped yet — so the source would restart forever and never advance past part 0.

The `parts[..next]` slice is also correct at both ends: empty before the first read, and
the whole list once `next == parts.len()`.

**Missing companions are skipped, not failures.** A declared companion that does not
exist contributes nothing and reads as current. Auxiliary files are best-effort; a
`jcode` session with no journal is the common case and must not become a read error.

### 3. jcode's merge, keyed by identity with the journal winning

`JcodeReader::ingest` currently begins with `self.responses.clear()`. Since a fresh
reader is constructed on every `reset`, that clear is redundant against the harness and
actively wrong for a group — it discards the snapshot when the journal arrives. Remove
it and merge by the existing dedup key (`jcode:{session}:{message_id}`) instead:

```rust
fn insert(&mut self, usage: ResponseUsage, dedup_key: String, max: usize) -> Result<(), ReaderError> {
    if let Some(&position) = self.index.get(&dedup_key) {
        // The journal is authoritative for a message it repeats.
        self.responses[position] = usage;
    } else {
        if self.responses.len() == max { return Err(ReaderError::Capacity("Jcode responses")); }
        self.index.insert(dedup_key, self.responses.len());
        self.responses.push(usage);
    }
    Ok(())
}
```

`index: HashMap<String, usize>` is new state on the reader; `retained_identity_bytes`
grows only for *new* keys, since a replacement retains no extra identity.

`ingest` also has to accept two shapes, because the two files differ:

- the snapshot is a **single JSON object** with a `messages` array;
- the journal is **JSONL**, one append batch per line.

Try `serde_json::from_slice::<Value>` first; on failure, split on newlines and treat each
line as an append batch. Both funnel into one `apply_batch(value, …)`.

Provider scoping follows the documented semantics: the snapshot's `provider_key`/`model`
are session-scoped, but each journal batch re-scopes them for its own messages. That is
why the model/provider are resolved per batch rather than once per document — a route
change late in a session must not authenticate earlier messages.

### 4. Change detection covers every constituent

Free once `is_current` is the `parts[..next]` check: a journal append makes the group
not-current, `read_step` requests a full reconciliation, `reset()` clears every part's
stamp and sets `next = 0`, and the whole group is re-read into a fresh reader. No
separate fingerprint over a set of files is needed.

## Tests

Implemented, in `tests/source_inputs.rs`, `tests/jcode.rs` and `tests/discovery.rs`:

1. `jcode` snapshot only — the message count and totals are unchanged.
2. `jcode` snapshot + journal appending a new message — counted once.
3. `jcode` snapshot + journal repeating a message id with different tokens — the
   journal's value wins and the message still appears once.
4. A declared companion that does not exist — the snapshot still reads, no error.
5. A companion growing between passes makes the group not-current, and `reset()` makes
   the next read start from the primary again.
6. The grouped read order itself: `reset` only on the first part, `more_available` on
   every non-final part, `is_current` true once both are stamped.
7. Acceptance fixture: still 100 / 1,000 / 2,784. No fixture source has a companion, so
   a move here would mean something is double-counting.

## What this unblocks, in order

1. **`jcode` sibling journal — done.** A merge into an existing reader, no new format.
2. **`roocode` — done, but not this way.** It is a single-file reader; see the correction
   at the top.
3. The VS Code `globalStorage` task lanes for `cline`, `kilo`, `copilot` and `kiro` —
   **done for `cline`, `kilo` and `copilot`, and none of them needed this.** The Cline
   family's task log is one file per task, so `src/parsers/roocode.rs` serves `roocode`,
   `cline` and `kilo` with the client as a parameter. Copilot's chat session is one
   operation log per session (`src/parsers/copilot_chat.rs`) and its CLI session state is
   one event log per session (`src/parsers/copilot_cli.rs`). `kiro`'s
   `globalStorage/kiro.kiroagent` lane has no documented shape and stays unread, but its
   SQLite store is now read and needs **no** grouping either — its conversations are
   dropped when a session file already reported them.
4. `prime-agent` fork reversal — **not needed, and this document was wrong to plan for it.**
   The pinned reader reverses a parent's `child_usage_attributed` aggregate so the copied
   parent can be deduplicated against the child's own transcript. This codebase never emits
   that aggregate: `PrimeAgentReader` skips it and the child's own transcript is a source in
   its own right, so the usage is already counted exactly once. Reversing an aggregate that
   is never emitted would be strictly worse. The residual limitation is the opposite one: a
   child whose artifacts were deleted while the parent's aggregate survives is not counted.

## Deliberately not included

- Concurrency. Constituents are read in order, one at a time, matching the sources'
  existing serial `step()` contract.
- Companions from outside the source's own directory. `monitored_source()` derives them
  by path arithmetic on the primary, so a pairing that needs cross-directory identity
  matching (Prime Agent) is out of scope here.
- `crush`, `warp`, `aside` and `hindsight`. They remain blocked on a missing root or a
  network boundary, which source groups does not address.
