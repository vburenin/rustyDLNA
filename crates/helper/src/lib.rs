//! Shared bounded primitives for external media helpers and daemon jobs.
//!
//! This crate owns the process-wide admission gates and the small amount of
//! unsafe Unix child setup used by scanner, probe, remux, and transcode work.
//! It also owns policy-neutral bounded I/O and time conversions that those
//! crates share. Media policy and command construction stay in their owners.

mod bounded_io;
mod cancellation;
mod gate;
mod process;
mod time;

pub use bounded_io::{read_to_end_bounded, BoundedReadError};
pub use cancellation::CancellationToken;
pub use gate::{HelperAdmissionError, HelperGate, HelperMetrics, HelperPermit, JobGate, JobPermit};
pub use process::{
    CaptureConfig, CaptureOverflow, CaptureReadError, CaptureRetention, CapturedStream,
    SupervisedCommand, SupervisedOutcome, SupervisedOutput, SupervisionError,
    DEFAULT_TERMINATION_GRACE,
};
pub use time::duration_millis_saturating;
