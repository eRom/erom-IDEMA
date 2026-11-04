//! Mesures du HUD : latence d'écho et durée des images (AC-9).

use std::collections::VecDeque;
use std::time::Duration;

/// Fenêtre glissante des dernières mesures.
#[derive(Debug)]
pub struct LatencyStats {
    samples: VecDeque<Duration>,
    capacity: usize,
}

impl LatencyStats {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    pub fn record(&mut self, sample: Duration) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Centile par la méthode du rang supérieur : `percentile(95.0)` sur 1..=100 ms donne 95 ms.
    pub fn percentile(&self, p: f64) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.samples.iter().copied().collect();
        sorted.sort();
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        Some(sorted[rank.clamp(1, sorted.len()) - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut stats = LatencyStats::new(100);
        for ms in (1..=100).rev() {
            stats.record(Duration::from_millis(ms));
        }
        assert_eq!(stats.percentile(50.0), Some(Duration::from_millis(50)));
        assert_eq!(stats.percentile(95.0), Some(Duration::from_millis(95)));
        assert_eq!(stats.percentile(100.0), Some(Duration::from_millis(100)));
    }

    #[test]
    fn window_keeps_only_the_latest_samples() {
        let mut stats = LatencyStats::new(3);
        for ms in [100, 1, 2, 3] {
            stats.record(Duration::from_millis(ms));
        }
        assert_eq!(stats.len(), 3);
        assert_eq!(stats.percentile(100.0), Some(Duration::from_millis(3)));
    }

    #[test]
    fn empty_window_has_no_percentile() {
        assert_eq!(LatencyStats::new(10).percentile(95.0), None);
    }
}
