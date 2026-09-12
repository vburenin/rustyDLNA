//! Bounded, process-local preparation measurements. Browser durations arrive
//! already elapsed; browser and server clock origins are never subtracted.
use super::AtomicDurationMetric;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_RECORDS: usize = 64;
const MAX_ATTEMPT_DURATIONS: usize = 3;
pub(crate) type TimingKey = (i64, u64, u64);

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum Stage {
    Preparation,
    SourceOpen,
    SourceSample,
    ToolIdentity,
    Admission,
    HelperAttempt,
    FirstCompleteFragment,
    SelectionToFrame,
    SeekToFrame,
    CapabilityNegotiation,
}
impl Stage {
    const ALL: [Self; 10] = [
        Self::Preparation,
        Self::SourceOpen,
        Self::SourceSample,
        Self::ToolIdentity,
        Self::Admission,
        Self::HelperAttempt,
        Self::FirstCompleteFragment,
        Self::SelectionToFrame,
        Self::SeekToFrame,
        Self::CapabilityNegotiation,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::Preparation => "preparation",
            Self::SourceOpen => "source_open",
            Self::SourceSample => "source_sample",
            Self::ToolIdentity => "tool_identity",
            Self::Admission => "admission",
            Self::HelperAttempt => "helper_attempt",
            Self::FirstCompleteFragment => "preparation_to_first_complete_fragment",
            Self::SelectionToFrame => "selection_to_frame",
            Self::SeekToFrame => "seek_to_frame",
            Self::CapabilityNegotiation => "capability_negotiation",
        }
    }
}

#[derive(Debug)]
struct Record {
    key: TimingKey,
    started: Instant,
    sequence: u64,
    stages: [Option<u64>; 10],
    attempts: u8,
    attempt_durations_ms: Vec<u64>,
}

#[derive(Debug)]
pub(crate) struct PerformanceMetrics {
    stages: [AtomicDurationMetric; 10],
    records: Mutex<VecDeque<Record>>,
    sequence: AtomicU64,
    pub(crate) fallbacks_hardware: AtomicU64,
    pub(crate) fallbacks_portable: AtomicU64,
}
impl Default for PerformanceMetrics {
    fn default() -> Self {
        Self {
            stages: std::array::from_fn(|_| AtomicDurationMetric::default()),
            records: Mutex::new(VecDeque::new()),
            sequence: AtomicU64::new(0),
            fallbacks_hardware: AtomicU64::new(0),
            fallbacks_portable: AtomicU64::new(0),
        }
    }
}
pub(crate) struct StageTimer<'a> {
    metrics: &'a PerformanceMetrics,
    key: Option<TimingKey>,
    stage: Stage,
    started: Instant,
}
impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        self.metrics
            .record(self.key, self.stage, self.started.elapsed());
    }
}
impl PerformanceMetrics {
    pub(crate) fn timer(&self, key: Option<TimingKey>, stage: Stage) -> StageTimer<'_> {
        self.timer_at(key, stage, Instant::now())
    }
    pub(crate) fn timer_at(
        &self,
        key: Option<TimingKey>,
        stage: Stage,
        started: Instant,
    ) -> StageTimer<'_> {
        StageTimer {
            metrics: self,
            key,
            stage,
            started,
        }
    }
    pub(crate) fn contains(&self, key: TimingKey) -> bool {
        crate::lock_recover(&self.records)
            .iter()
            .any(|record| record.key == key)
    }

    #[cfg(test)]
    pub(crate) fn begin(&self, key: TimingKey) {
        self.begin_at(key, Instant::now());
    }
    pub(crate) fn begin_at(&self, key: TimingKey, started: Instant) {
        let mut records = crate::lock_recover(&self.records);
        if records.iter().any(|record| record.key == key) {
            return;
        }
        if records.len() == MAX_RECORDS {
            records.pop_front();
        }
        records.push_back(Record {
            key,
            started,
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            stages: [None; 10],
            attempts: 0,
            attempt_durations_ms: Vec::new(),
        });
    }
    pub(crate) fn record(&self, key: Option<TimingKey>, stage: Stage, duration: Duration) {
        let mut records = crate::lock_recover(&self.records);
        if let Some(record) =
            key.and_then(|key| records.iter_mut().find(|record| record.key == key))
        {
            if matches!(stage, Stage::HelperAttempt) {
                record.attempts = record.attempts.saturating_add(1);
                if record.attempt_durations_ms.len() < MAX_ATTEMPT_DURATIONS {
                    record
                        .attempt_durations_ms
                        .push(rusty_dlna_helper::duration_millis_saturating(duration));
                }
            } else if !matches!(stage, Stage::SeekToFrame)
                && record.stages[stage as usize].is_some()
            {
                return;
            }
            record.stages[stage as usize] =
                Some(rusty_dlna_helper::duration_millis_saturating(duration));
        }
        drop(records);
        self.stages[stage as usize].record(duration);
    }
    pub(crate) fn since_entry_at(&self, key: TimingKey, stage: Stage, observed: Instant) {
        let elapsed = crate::lock_recover(&self.records)
            .iter()
            .find(|record| record.key == key)
            .map(|record| observed.saturating_duration_since(record.started));
        if let Some(elapsed) = elapsed {
            self.record(Some(key), stage, elapsed);
        }
    }
    pub(crate) fn snapshot(&self) -> serde_json::Value {
        let stages: serde_json::Map<String, serde_json::Value> = Stage::ALL
            .into_iter()
            .map(|stage| {
                (
                    stage.name().to_owned(),
                    serde_json::to_value(self.stages[stage as usize].snapshot())
                        .unwrap_or_default(),
                )
            })
            .collect();
        let records = crate::lock_recover(&self.records).iter().map(|record| {
            let stages: serde_json::Map<String, serde_json::Value> = Stage::ALL.into_iter()
                .filter_map(|stage| record.stages[stage as usize].map(|duration| (stage.name().to_owned(), duration.into()))).collect();
            serde_json::json!({ "sequence": record.sequence, "stages_ms": stages, "helper_attempts": record.attempts, "helper_attempt_durations_ms": record.attempt_durations_ms })
        }).collect::<Vec<_>>();
        serde_json::json!({ "bucket_bounds_ms": super::DURATION_BUCKET_BOUNDS_MS, "stages_ms": stages,
            "recent": records, "recent_limit": MAX_RECORDS,
            "fallbacks": { "hardware_total": self.fallbacks_hardware.load(Ordering::Relaxed),
                "portable_total": self.fallbacks_portable.load(Ordering::Relaxed) } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timing_records_and_histograms_have_fixed_bounds_and_deduplicate_phases() {
        let metrics = PerformanceMetrics::default();
        for i in 0..1000 {
            let key = (i, 2, 3);
            metrics.begin(key);
            metrics.record(Some(key), Stage::SourceOpen, Duration::from_millis(7));
            metrics.record(Some(key), Stage::SourceOpen, Duration::from_millis(7));
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["recent"].as_array().unwrap().len(), MAX_RECORDS);
        let metric = metrics.stages[Stage::SourceOpen as usize].snapshot();
        assert_eq!(metric.count, 1000);
        assert_eq!(metric.buckets.iter().sum::<u64>(), metric.count);
        assert_eq!(metric.buckets[3], 1000);
        assert!(!snapshot.to_string().contains("request_id"));
    }
    #[test]
    fn preparation_entry_and_fragment_observation_keep_distinct_monotonic_phases() {
        let metrics = PerformanceMetrics::default();
        let key = (42, 7, 11);
        let entered = Instant::now() - Duration::from_millis(200);
        metrics.begin_at(key, entered);
        drop(metrics.timer_at(Some(key), Stage::Preparation, entered));
        let fragment = entered + Duration::from_millis(50);
        metrics.since_entry_at(key, Stage::FirstCompleteFragment, fragment);
        metrics.since_entry_at(
            key,
            Stage::FirstCompleteFragment,
            fragment + Duration::from_secs(1),
        );
        let snapshot = metrics.snapshot();
        assert!(
            snapshot["recent"][0]["stages_ms"]["preparation"]
                .as_u64()
                .unwrap()
                >= 200
        );
        assert_eq!(
            snapshot["recent"][0]["stages_ms"]["preparation_to_first_complete_fragment"],
            50
        );
        assert_eq!(
            snapshot["stages_ms"]["preparation_to_first_complete_fragment"]["count"],
            1
        );
    }

    #[test]
    fn helper_attempt_details_are_bounded_while_histogram_counts_every_attempt() {
        let metrics = PerformanceMetrics::default();
        let key = (42, 7, 11);
        metrics.begin(key);
        for duration in 1..=20 {
            metrics.record(
                Some(key),
                Stage::HelperAttempt,
                Duration::from_millis(duration),
            );
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["recent"][0]["helper_attempts"], 20);
        assert_eq!(
            snapshot["recent"][0]["helper_attempt_durations_ms"],
            serde_json::json!([1, 2, 3])
        );
        assert_eq!(snapshot["stages_ms"]["helper_attempt"]["count"], 20);
        assert_eq!(snapshot["stages_ms"]["helper_attempt"]["sum_ms"], 210);
    }
}
