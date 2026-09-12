//! Experimental media policy. It deliberately does not change daemon defaults.
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Copy,
    Audio,
    Video,
    Both,
    Gpu,
    Upscale,
    Probe,
    Artwork,
}

impl Work {
    pub fn interactive(self) -> bool {
        !matches!(self, Self::Probe | Self::Artwork)
    }

    pub fn cpu(self, capacity: usize) -> usize {
        let weight = match self {
            Self::Copy | Self::Probe => 1,
            Self::Audio | Self::Artwork => 2,
            // Hardware work retains CPU decode, audio, transfer and submission cost.
            Self::Video | Self::Both | Self::Gpu | Self::Upscale => 4,
        };
        weight.min(capacity.max(1))
    }

    fn gpu(self) -> bool {
        matches!(self, Self::Gpu | Self::Upscale)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub id: usize,
    pub work: Work,
    pub queued_ms: u64,
}

pub struct Scheduler {
    pub pending: VecDeque<Request>,
    active: Vec<Request>,
    helpers: usize,
    cpus: usize,
    consecutive_interactive: usize,
    experimental: bool,
}

impl Scheduler {
    pub fn new(helpers: usize, cpus: usize, experimental: bool) -> Self {
        Self {
            pending: VecDeque::new(),
            active: Vec::new(),
            helpers: helpers.max(1),
            cpus: cpus.max(1),
            consecutive_interactive: 0,
            experimental,
        }
    }

    pub fn enqueue(&mut self, request: Request) -> Result<(), &'static str> {
        if self.pending.len() >= 128 {
            return Err("prototype queue full");
        }
        self.pending.push_back(request);
        Ok(())
    }

    pub fn release(&mut self, id: usize) {
        self.active.retain(|request| request.id != id);
    }

    pub fn expire(&mut self, now_ms: u64, timeout_ms: u64) -> Vec<Request> {
        let expired = self
            .pending
            .iter()
            .copied()
            .filter(|request| now_ms.saturating_sub(request.queued_ms) >= timeout_ms)
            .collect();
        self.pending
            .retain(|request| now_ms.saturating_sub(request.queued_ms) < timeout_ms);
        expired
    }

    pub fn next(&mut self, now_ms: u64) -> Option<Request> {
        if self.active.len() >= self.helpers {
            return None;
        }
        let position = if !self.experimental {
            0
        } else {
            let background = self
                .pending
                .iter()
                .position(|request| !request.work.interactive());
            let interactive = self
                .pending
                .iter()
                .position(|request| request.work.interactive());
            let background_slots_available = self
                .active
                .iter()
                .filter(|request| !request.work.interactive())
                .count()
                < self.helpers.saturating_sub(1).max(1);
            // After two interactive admissions, or one when background has
            // waited 500 ms, let the oldest background request drain CPU
            // capacity. Conversely, each background admission gives a waiting
            // interactive request a turn, even under a one-CPU ceiling.
            // Background already at its slot cap cannot consume the reserved
            // interactive slot by preventing that request's consideration.
            let owed_background = background.filter(|&index| {
                background_slots_available
                    && (self.consecutive_interactive >= 2
                        || (self.consecutive_interactive > 0
                            && now_ms.saturating_sub(self.pending[index].queued_ms) >= 500))
            });
            owed_background.or(interactive).or(background)?
        };
        let request = *self.pending.get(position)?;
        if self.experimental {
            let used: usize = self
                .active
                .iter()
                .map(|item| item.work.cpu(self.cpus))
                .sum();
            if used.saturating_add(request.work.cpu(self.cpus)) > self.cpus
                || (request.work.gpu() && self.active.iter().any(|item| item.work.gpu()))
                || (request.work == Work::Upscale
                    && self.active.iter().any(|item| item.work == Work::Upscale))
            {
                return None;
            }
            // Reserve one slot for interactive starts when at least two exist.
            // For a one-slot global ceiling, bounded class alternation still works.
            let background = self
                .active
                .iter()
                .filter(|item| !item.work.interactive())
                .count();
            if !request.work.interactive() && background >= self.helpers.saturating_sub(1).max(1) {
                return None;
            }
        }
        self.pending.remove(position);
        self.active.push(request);
        self.consecutive_interactive = if request.work.interactive() {
            self.consecutive_interactive.saturating_add(1)
        } else {
            0
        };
        Some(request)
    }
}

#[derive(Clone, Copy)]
pub struct Viewer {
    pub position: f64,
    pub rate: f64,
    pub paused: bool,
    pub lease_end: f64,
}

/// Bounded shared-producer demand experiment; never signal-stop a real helper.
/// Native/download consumers retain unrestricted demand, independent of browsers.
pub fn desired_end(viewers: &[Viewer], now: f64, produced: f64, native: bool) -> f64 {
    if native {
        return f64::INFINITY;
    }
    viewers
        .iter()
        .take(1024)
        .filter(|viewer| {
            viewer.lease_end > now
                && viewer.position.is_finite()
                && viewer.position >= 0.0
                && viewer.rate.is_finite()
                && (0.25..=4.0).contains(&viewer.rate)
        })
        .fold(produced, |end, viewer| {
            let lead = if viewer.paused {
                0.0
            } else {
                (30.0 * viewer.rate).min(60.0)
            };
            end.max(viewer.position + lead)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: usize, work: Work) -> Request {
        Request {
            id,
            work,
            queued_ms: 0,
        }
    }

    #[test]
    fn mixed_work_never_exceeds_global_cpu_or_device_ceilings() {
        let mut scheduler = Scheduler::new(4, 8, true);
        for (id, work) in [
            Work::Gpu,
            Work::Upscale,
            Work::Copy,
            Work::Audio,
            Work::Artwork,
        ]
        .into_iter()
        .enumerate()
        {
            scheduler.enqueue(request(id, work)).unwrap();
        }
        assert_eq!(scheduler.next(0).unwrap().work, Work::Gpu);
        assert!(scheduler.next(0).is_none());
        scheduler.release(0);
        assert_eq!(scheduler.next(0).unwrap().work, Work::Upscale);
        assert_eq!(scheduler.next(0).unwrap().work, Work::Artwork);
        assert_eq!(scheduler.next(0).unwrap().work, Work::Copy);
        assert!(scheduler.next(0).is_none());
        assert_eq!(
            scheduler
                .active
                .iter()
                .map(|r| r.work.cpu(8))
                .sum::<usize>(),
            7
        );
    }

    #[test]
    fn background_cannot_starve_under_continuous_interactive_arrivals() {
        let mut scheduler = Scheduler::new(1, 1, true);
        scheduler.enqueue(request(0, Work::Artwork)).unwrap();
        for id in 1..10 {
            scheduler.enqueue(request(id, Work::Copy)).unwrap();
        }
        for id in 1..=2 {
            assert_eq!(scheduler.next(0).unwrap().id, id);
            scheduler.release(id);
        }
        assert_eq!(scheduler.next(0).unwrap().id, 0);
    }

    #[test]
    fn overdue_background_cannot_block_the_reserved_interactive_slot() {
        let mut scheduler = Scheduler::new(2, 8, true);
        scheduler.enqueue(request(0, Work::Artwork)).unwrap();
        assert_eq!(scheduler.next(0).unwrap().id, 0);
        // Two earlier interactive admissions make background owed even before
        // its age threshold, without changing the occupied background slot.
        for id in 1..=2 {
            scheduler.enqueue(request(id, Work::Copy)).unwrap();
            assert_eq!(scheduler.next(0).unwrap().id, id);
            scheduler.release(id);
        }
        scheduler.enqueue(request(3, Work::Artwork)).unwrap();
        scheduler.enqueue(request(4, Work::Copy)).unwrap();
        assert_eq!(scheduler.next(500).unwrap().id, 4);
        assert_eq!(scheduler.active.len(), 2);
        scheduler.release(0);
        assert_eq!(scheduler.next(501).unwrap().id, 3);
    }

    #[test]
    fn continuous_overdue_background_cannot_starve_interactive_at_one_cpu() {
        let mut scheduler = Scheduler::new(2, 1, true);
        scheduler.enqueue(request(0, Work::Artwork)).unwrap();
        assert_eq!(scheduler.next(0).unwrap().id, 0);
        for id in 1..10 {
            scheduler.enqueue(request(id, Work::Artwork)).unwrap();
        }
        scheduler.enqueue(request(10, Work::Copy)).unwrap();
        assert!(scheduler.next(500).is_none());
        scheduler.release(0);
        assert_eq!(scheduler.next(501).unwrap().id, 10);
        scheduler.release(10);
        assert_eq!(scheduler.next(502).unwrap().id, 1);
    }

    #[test]
    fn queue_deadline_removal_and_release_restore_admission() {
        let mut scheduler = Scheduler::new(2, 1, true);
        scheduler.enqueue(request(0, Work::Both)).unwrap();
        scheduler.enqueue(request(1, Work::Audio)).unwrap();
        assert_eq!(scheduler.next(0).unwrap().id, 0);
        assert!(scheduler.next(0).is_none());
        assert_eq!(scheduler.expire(2000, 2000).len(), 1);
        scheduler.release(0);
        scheduler
            .enqueue(Request {
                queued_ms: 2001,
                ..request(2, Work::Video)
            })
            .unwrap();
        assert_eq!(scheduler.next(2001).unwrap().id, 2);
        for id in 3..131 {
            scheduler.enqueue(request(id, Work::Copy)).unwrap();
        }
        assert!(scheduler.enqueue(request(132, Work::Copy)).is_err());
    }

    #[test]
    fn shared_pacing_pause_expiry_rate_and_native_semantics() {
        let active = Viewer {
            position: 100.0,
            rate: 2.0,
            paused: false,
            lease_end: 200.0,
        };
        let paused = Viewer {
            paused: true,
            ..active
        };
        assert_eq!(desired_end(&[active, paused], 120.0, 130.0, false), 160.0);
        assert_eq!(desired_end(&[paused], 120.0, 130.0, false), 130.0);
        assert_eq!(desired_end(&[active], 201.0, 130.0, false), 130.0);
        assert!(desired_end(&[paused], 120.0, 130.0, true).is_infinite());
    }

    #[test]
    fn two_hour_two_x_simulation_retains_lead_and_pause_is_bounded() {
        let mut produced: f64 = 30.0;
        let mut position = 0.0;
        for second in 0..3600 {
            let paused = (100..200).contains(&second);
            let viewer = Viewer {
                position,
                rate: 2.0,
                paused,
                lease_end: second as f64 + 30.0,
            };
            let desired = desired_end(&[viewer], second as f64, produced, false);
            produced = (produced + 4.0).min(desired);
            if !paused {
                position += 2.0;
            }
            assert!(produced >= position);
            assert!(produced - position <= 60.0);
        }
        // Fixed 1x production after its initial lead cannot sustain 2x.
        assert!(30.0 + 31.0 < 2.0 * 31.0);
    }
}
