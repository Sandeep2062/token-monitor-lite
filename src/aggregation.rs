//! Canonical checked rollups over complete immutable source snapshots.
//! Stable parser identities deduplicate supported cross-file copies. Unkeyed
//! rows are never guessed to be duplicates, and conflicts are withheld.

use crate::accounting::{SessionKey, TokenCounts, TokenDelta};
use crate::parsers::ResponseUsage;
use crate::sources::{
    ObservationSourceState, RetainedPeriodUsage, SourceFormat, SourceId, SourceSnapshot,
};
use chrono::{DateTime, NaiveDate};
use chrono_tz::Tz;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;

pub const MAX_SOURCES: usize = 4_096;
pub const MAX_CANONICAL_ROWS: usize = 262_144;

/// Validated named IANA timezone used for stable local-day bucketing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketTimezone(Tz);

impl BucketTimezone {
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    pub(crate) fn tz(self) -> Tz {
        self.0
    }
}

impl FromStr for BucketTimezone {
    type Err = chrono_tz::ParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceRef {
    pub source_id: SourceId,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceProvenance {
    pub source: SourceRef,
    pub observed_rows: usize,
    /// Rows whose identity appears only in this source observation.
    pub unique_rows: usize,
    /// Rows participating in an identity observed more than once.
    pub overlap_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayModelRollup {
    pub day: NaiveDate,
    pub client: String,
    pub model: String,
    pub tokens: TokenCounts,
    pub cache_write_1h: u64,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub sources: Arc<[SourceRef]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionModelRollup {
    pub key: SessionKey,
    pub model: String,
    pub tokens: TokenCounts,
    pub cache_write_1h: u64,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub sources: Arc<[SourceRef]>,
}

/// Canonical session totals for one local calendar day. Used to apply
/// archive replacement per period without re-reading source rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDayRollup {
    pub day: NaiveDate,
    pub key: SessionKey,
    pub tokens: TokenCounts,
    pub cache_write_1h: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalSnapshot {
    pub generation: u64,
    pub timezone_name: String,
    pub totals: TokenCounts,
    pub cache_write_1h: u64,
    pub day_models: Arc<[DayModelRollup]>,
    pub session_models: Arc<[SessionModelRollup]>,
    pub session_days: Arc<[SessionDayRollup]>,
    pub retained_periods: Arc<[RetainedPeriodUsage]>,
    pub sources: Arc<[SourceProvenance]>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CanonicalProvenanceLink {
    pub source_id: SourceId,
    pub source_generation: u64,
    pub observation_ordinal: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalObservation {
    pub canonical_id: String,
    pub usage: ResponseUsage,
    pub source_state: ObservationSourceState,
    pub provenance: Arc<[CanonicalProvenanceLink]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalObservationSnapshot {
    pub generation: u64,
    pub rows: Arc<[CanonicalObservation]>,
    pub rollups: Arc<CanonicalSnapshot>,
}

impl CanonicalSnapshot {
    fn empty(timezone: BucketTimezone) -> Self {
        Self {
            generation: 0,
            timezone_name: timezone.name().to_owned(),
            totals: TokenCounts::default(),
            cache_write_1h: 0,
            day_models: Arc::from([]),
            session_models: Arc::from([]),
            session_days: Arc::from([]),
            retained_periods: Arc::from([]),
            sources: Arc::from([]),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanonicalDelta {
    pub tokens: TokenDelta,
    pub cache_write_1h: i128,
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum AggregationError {
    #[error("canonical aggregation capacity exceeded ({0})")]
    Capacity(&'static str),
    #[error("canonical aggregation overflow")]
    Overflow,
    #[error("canonical source snapshot is inconsistent")]
    SourceMismatch,
    #[error("conflicting records share a stable overlap identity")]
    ConflictingOverlap,
    #[error("invalid response timestamp")]
    InvalidTimestamp,
    #[error("stale source generation")]
    StaleGeneration,
    #[error("canonical generation overflow")]
    GenerationOverflow,
}

/// Holds only successfully aggregated sources. A failed replacement leaves
/// both the source set and the immutable last-good snapshot untouched.
pub struct CanonicalLedger {
    timezone: BucketTimezone,
    sources: BTreeMap<SourceId, Arc<SourceSnapshot>>,
    canonical: Arc<CanonicalObservationSnapshot>,
    last_good: Arc<CanonicalSnapshot>,
}

pub struct CanonicalCandidate {
    base_generation: u64,
    source: Arc<SourceSnapshot>,
    sources: BTreeMap<SourceId, Arc<SourceSnapshot>>,
    canonical: Arc<CanonicalObservationSnapshot>,
    delta: CanonicalDelta,
}

impl CanonicalCandidate {
    pub fn source(&self) -> &Arc<SourceSnapshot> {
        &self.source
    }

    pub fn canonical(&self) -> Arc<CanonicalObservationSnapshot> {
        self.canonical.clone()
    }

    pub fn delta(&self) -> CanonicalDelta {
        self.delta
    }
}

impl CanonicalLedger {
    pub fn new(timezone: BucketTimezone) -> Self {
        let rollups = Arc::new(CanonicalSnapshot::empty(timezone));
        Self {
            timezone,
            sources: BTreeMap::new(),
            canonical: Arc::new(CanonicalObservationSnapshot {
                generation: 0,
                rows: Arc::from([]),
                rollups: rollups.clone(),
            }),
            last_good: rollups,
        }
    }

    pub fn last_good(&self) -> Arc<CanonicalSnapshot> {
        self.last_good.clone()
    }

    pub fn canonical(&self) -> Arc<CanonicalObservationSnapshot> {
        self.canonical.clone()
    }

    pub fn prepare_replace_source(
        &self,
        source: Arc<SourceSnapshot>,
    ) -> Result<CanonicalCandidate, AggregationError> {
        if let Some(previous) = self.sources.get(&source.source_id)
            && source.generation <= previous.generation
        {
            return Err(AggregationError::StaleGeneration);
        }
        if !self.sources.contains_key(&source.source_id) && self.sources.len() == MAX_SOURCES {
            return Err(AggregationError::Capacity("source count"));
        }

        let mut candidate = self.sources.clone();
        candidate.insert(source.source_id, source.clone());
        let generation = self
            .last_good
            .generation
            .checked_add(1)
            .ok_or(AggregationError::GenerationOverflow)?;
        let canonical = Arc::new(aggregate_sources(&candidate, self.timezone, generation)?);
        let delta = CanonicalDelta {
            tokens: canonical.rollups.totals.difference(self.last_good.totals),
            cache_write_1h: i128::from(canonical.rollups.cache_write_1h)
                - i128::from(self.last_good.cache_write_1h),
        };
        Ok(CanonicalCandidate {
            base_generation: self.last_good.generation,
            source,
            sources: candidate,
            canonical,
            delta,
        })
    }

    pub fn activate(
        &mut self,
        candidate: CanonicalCandidate,
    ) -> Result<CanonicalDelta, AggregationError> {
        if candidate.base_generation != self.last_good.generation {
            return Err(AggregationError::StaleGeneration);
        }
        self.sources = candidate.sources;
        self.last_good = candidate.canonical.rollups.clone();
        self.canonical = candidate.canonical;
        Ok(candidate.delta)
    }

    pub fn replace_source(
        &mut self,
        source: Arc<SourceSnapshot>,
    ) -> Result<CanonicalDelta, AggregationError> {
        let candidate = self.prepare_replace_source(source)?;
        self.activate(candidate)
    }

    pub fn rehydrate(
        timezone: BucketTimezone,
        sources: impl IntoIterator<Item = Arc<SourceSnapshot>>,
        generation: u64,
    ) -> Result<Self, AggregationError> {
        let sources = sources
            .into_iter()
            .map(|source| (source.source_id, source))
            .collect::<BTreeMap<_, _>>();
        if sources.len() > MAX_SOURCES {
            return Err(AggregationError::Capacity("source count"));
        }
        let canonical = Arc::new(aggregate_sources(&sources, timezone, generation)?);
        Ok(Self {
            timezone,
            sources,
            last_good: canonical.rollups.clone(),
            canonical,
        })
    }
}

struct CanonicalRow {
    canonical_id: String,
    usage: ResponseUsage,
    sources: BTreeSet<SourceRef>,
    source_indexes: Vec<usize>,
    provenance: Vec<CanonicalProvenanceLink>,
}

#[derive(Default)]
struct RollupAccumulator {
    tokens: TokenCounts,
    cache_write_1h: u64,
    first_seen_ms: Option<i64>,
    last_seen_ms: Option<i64>,
    sources: BTreeSet<SourceRef>,
}

impl RollupAccumulator {
    fn add(&mut self, row: &CanonicalRow) -> Result<(), AggregationError> {
        self.tokens = self
            .tokens
            .checked_add(row.usage.tokens)
            .map_err(|_| AggregationError::Overflow)?;
        self.cache_write_1h = self
            .cache_write_1h
            .checked_add(row.usage.cache_write_1h)
            .ok_or(AggregationError::Overflow)?;
        self.first_seen_ms = Some(self.first_seen_ms.map_or(row.usage.timestamp_ms, |old| {
            old.min(row.usage.timestamp_ms)
        }));
        self.last_seen_ms = Some(self.last_seen_ms.map_or(row.usage.timestamp_ms, |old| {
            old.max(row.usage.timestamp_ms)
        }));
        self.sources.extend(row.sources.iter().cloned());
        Ok(())
    }
}

#[derive(Default)]
struct TokenAccumulator {
    tokens: TokenCounts,
    cache_write_1h: u64,
}

impl TokenAccumulator {
    fn add(&mut self, row: &CanonicalRow) -> Result<(), AggregationError> {
        self.tokens = self
            .tokens
            .checked_add(row.usage.tokens)
            .map_err(|_| AggregationError::Overflow)?;
        self.cache_write_1h = self
            .cache_write_1h
            .checked_add(row.usage.cache_write_1h)
            .ok_or(AggregationError::Overflow)?;
        Ok(())
    }
}

fn aggregate_sources(
    sources: &BTreeMap<SourceId, Arc<SourceSnapshot>>,
    timezone: BucketTimezone,
    generation: u64,
) -> Result<CanonicalObservationSnapshot, AggregationError> {
    let observed_rows = sources.values().try_fold(0_usize, |count, source| {
        count
            .checked_add(source.rows.len())
            .ok_or(AggregationError::Capacity("row count"))
    })?;
    if observed_rows > MAX_CANONICAL_ROWS {
        return Err(AggregationError::Capacity("row count"));
    }

    let mut provenance: Vec<SourceProvenance> = sources
        .values()
        .map(|source| SourceProvenance {
            source: source_ref(source),
            observed_rows: source.rows.len(),
            unique_rows: 0,
            overlap_rows: 0,
        })
        .collect();
    let mut canonical: Vec<CanonicalRow> = Vec::with_capacity(observed_rows);
    let mut stable: BTreeMap<(String, String), usize> = BTreeMap::new();
    let native_codex_sessions: BTreeSet<String> = sources
        .values()
        .filter(|source| source.format == SourceFormat::CodexCli)
        .flat_map(|source| source.rows.iter())
        .filter(|row| row.key.client == "codex")
        .map(|row| row.key.session.clone())
        .collect();

    for (source_index, source) in sources.values().enumerate() {
        validate_source(source)?;
        let reference = source_ref(source);
        for (observation_ordinal, usage) in source.rows.iter().enumerate() {
            // A supplemental ledger row for a thread already represented by
            // an exact native rollout is ignored, never added.
            if source.format == SourceFormat::CodexLocalUsageSqlite
                && native_codex_sessions.contains(&usage.key.session)
            {
                continue;
            }
            let link = CanonicalProvenanceLink {
                source_id: source.source_id,
                source_generation: source.generation,
                observation_ordinal,
            };
            if let Some(key) = usage.dedup_key.as_ref() {
                let identity = (usage.key.client.clone(), key.clone());
                if let Some(index) = stable.get(&identity).copied() {
                    merge_overlap(&mut canonical[index].usage, usage)?;
                    canonical[index].sources.insert(reference.clone());
                    canonical[index].source_indexes.push(source_index);
                    canonical[index].provenance.push(link);
                    continue;
                }
                stable.insert(identity, canonical.len());
            }
            let mut row_sources = BTreeSet::new();
            row_sources.insert(reference.clone());
            canonical.push(CanonicalRow {
                canonical_id: canonical_identity(source.source_id, observation_ordinal, usage),
                usage: usage.clone(),
                sources: row_sources,
                source_indexes: vec![source_index],
                provenance: vec![link],
            });
        }
    }

    for row in &canonical {
        let overlaps = row.source_indexes.len() > 1;
        for source_index in &row.source_indexes {
            if overlaps {
                provenance[*source_index].overlap_rows += 1;
            } else {
                provenance[*source_index].unique_rows += 1;
            }
        }
    }

    let mut totals = TokenCounts::default();
    let mut cache_write_1h = 0_u64;
    let mut days: BTreeMap<(NaiveDate, String, String), RollupAccumulator> = BTreeMap::new();
    let mut sessions: BTreeMap<(String, String, String), RollupAccumulator> = BTreeMap::new();
    let mut session_days: BTreeMap<(NaiveDate, String, String), TokenAccumulator> = BTreeMap::new();
    for row in &canonical {
        totals = totals
            .checked_add(row.usage.tokens)
            .map_err(|_| AggregationError::Overflow)?;
        cache_write_1h = cache_write_1h
            .checked_add(row.usage.cache_write_1h)
            .ok_or(AggregationError::Overflow)?;
        let utc = DateTime::from_timestamp_millis(row.usage.timestamp_ms)
            .ok_or(AggregationError::InvalidTimestamp)?;
        let day = utc.with_timezone(&timezone.0).date_naive();
        days.entry((day, row.usage.key.client.clone(), row.usage.model.clone()))
            .or_default()
            .add(row)?;
        sessions
            .entry((
                row.usage.key.client.clone(),
                row.usage.key.session.clone(),
                row.usage.model.clone(),
            ))
            .or_default()
            .add(row)?;
        session_days
            .entry((
                day,
                row.usage.key.client.clone(),
                row.usage.key.session.clone(),
            ))
            .or_default()
            .add(row)?;
    }

    let day_models = days
        .into_iter()
        .map(|((day, client, model), value)| DayModelRollup {
            day,
            client,
            model,
            tokens: value.tokens,
            cache_write_1h: value.cache_write_1h,
            first_seen_ms: value.first_seen_ms.expect("nonempty accumulator"),
            last_seen_ms: value.last_seen_ms.expect("nonempty accumulator"),
            sources: value.sources.into_iter().collect::<Vec<_>>().into(),
        })
        .collect::<Vec<_>>();
    let session_models = sessions
        .into_iter()
        .map(|((client, session, model), value)| SessionModelRollup {
            key: SessionKey::new(client, session),
            model,
            tokens: value.tokens,
            cache_write_1h: value.cache_write_1h,
            first_seen_ms: value.first_seen_ms.expect("nonempty accumulator"),
            last_seen_ms: value.last_seen_ms.expect("nonempty accumulator"),
            sources: value.sources.into_iter().collect::<Vec<_>>().into(),
        })
        .collect::<Vec<_>>();
    let session_days = session_days
        .into_iter()
        .map(|((day, client, session), value)| SessionDayRollup {
            day,
            key: SessionKey::new(client, session),
            tokens: value.tokens,
            cache_write_1h: value.cache_write_1h,
        })
        .collect::<Vec<_>>();

    let mut retained_periods = BTreeMap::new();
    for source in sources.values() {
        for row in source.retained.iter() {
            let key = (
                row.period.as_str().to_owned(),
                row.client.clone(),
                row.session.clone(),
            );
            match retained_periods.get(&key) {
                None => {
                    retained_periods.insert(key, row.clone());
                }
                Some(existing) if row.captured_at_ms > existing.captured_at_ms => {
                    retained_periods.insert(key, row.clone());
                }
                Some(existing)
                    if row.captured_at_ms == existing.captured_at_ms && existing != row =>
                {
                    return Err(AggregationError::ConflictingOverlap);
                }
                Some(_) => {}
            }
        }
    }

    let rollups = Arc::new(CanonicalSnapshot {
        generation,
        timezone_name: timezone.name().to_owned(),
        totals,
        cache_write_1h,
        day_models: day_models.into(),
        session_models: session_models.into(),
        session_days: session_days.into(),
        retained_periods: retained_periods.into_values().collect::<Vec<_>>().into(),
        sources: provenance.into(),
    });
    let rows = canonical
        .into_iter()
        .map(|row| CanonicalObservation {
            canonical_id: row.canonical_id,
            source_state: if row.provenance.iter().any(|link| {
                sources
                    .get(&link.source_id)
                    .is_some_and(|source| source.source_state == ObservationSourceState::Live)
            }) {
                ObservationSourceState::Live
            } else {
                ObservationSourceState::Retained
            },
            usage: row.usage,
            provenance: row.provenance.into(),
        })
        .collect::<Vec<_>>();
    Ok(CanonicalObservationSnapshot {
        generation,
        rows: rows.into(),
        rollups,
    })
}

fn canonical_identity(source_id: SourceId, ordinal: usize, usage: &ResponseUsage) -> String {
    usage.dedup_key.as_ref().map_or_else(
        || format!("local:{}:{ordinal}", source_id.as_storage_key()),
        |identity| format!("stable:{}:{identity}", usage.key.client),
    )
}

fn merge_overlap(
    existing: &mut ResponseUsage,
    candidate: &ResponseUsage,
) -> Result<(), AggregationError> {
    if existing.key.client != "claude" {
        if existing != candidate {
            return Err(AggregationError::ConflictingOverlap);
        }
        return Ok(());
    }

    if existing.key.client != candidate.key.client
        || existing.model != candidate.model
        || existing.timestamp_ms != candidate.timestamp_ms
        || existing.dedup_key != candidate.dedup_key
        || existing.cost != candidate.cost
    {
        return Err(AggregationError::ConflictingOverlap);
    }
    merge_optional_identity(
        &mut existing.billing_provider_hint,
        &candidate.billing_provider_hint,
    )?;
    merge_optional_identity(&mut existing.service_tier, &candidate.service_tier)?;

    let existing_has_cache = existing.tokens.cache_read > 0 || existing.tokens.cache_write > 0;
    let candidate_has_cache = candidate.tokens.cache_read > 0 || candidate.tokens.cache_write > 0;
    existing.tokens.input = match (existing_has_cache, candidate_has_cache) {
        (true, false) if existing.tokens.input > 0 => existing.tokens.input,
        (false, true) if candidate.tokens.input > 0 => candidate.tokens.input,
        _ => existing.tokens.input.max(candidate.tokens.input),
    };
    existing.tokens.output = existing.tokens.output.max(candidate.tokens.output);
    existing.tokens.cache_read = existing.tokens.cache_read.max(candidate.tokens.cache_read);
    existing.tokens.cache_write = existing
        .tokens
        .cache_write
        .max(candidate.tokens.cache_write);
    existing.tokens.reasoning = existing.tokens.reasoning.max(candidate.tokens.reasoning);
    existing.cache_write_1h = existing
        .cache_write_1h
        .max(candidate.cache_write_1h)
        .min(existing.tokens.cache_write);

    if candidate.key.session < existing.key.session {
        existing.key.session.clone_from(&candidate.key.session);
    }
    Ok(())
}

fn merge_optional_identity(
    existing: &mut Option<String>,
    candidate: &Option<String>,
) -> Result<(), AggregationError> {
    match (&*existing, candidate) {
        (Some(left), Some(right)) if left != right => Err(AggregationError::ConflictingOverlap),
        (None, Some(_)) => {
            existing.clone_from(candidate);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_source(source: &SourceSnapshot) -> Result<(), AggregationError> {
    let mut totals = TokenCounts::default();
    let mut cache_write_1h = 0_u64;
    for row in source.rows.iter() {
        let client_allowed = row.key.client == source.format.client()
            || (source.format == SourceFormat::ClaudeCli
                && (row.key.client == "claude-desktop"
                    || row.key.client.starts_with("cc-mirror/")));
        if !client_allowed {
            return Err(AggregationError::SourceMismatch);
        }
        totals = totals
            .checked_add(row.tokens)
            .map_err(|_| AggregationError::Overflow)?;
        cache_write_1h = cache_write_1h
            .checked_add(row.cache_write_1h)
            .ok_or(AggregationError::Overflow)?;
    }
    if totals != source.totals || cache_write_1h != source.cache_write_1h {
        return Err(AggregationError::SourceMismatch);
    }
    Ok(())
}

fn source_ref(source: &SourceSnapshot) -> SourceRef {
    SourceRef {
        source_id: source.source_id,
        generation: source.generation,
    }
}
