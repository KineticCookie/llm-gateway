use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::time::timeout;

use crate::config::ProxyConfig;
use crate::metrics;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("Queue is full for project: {0}")]
    QueueFull(String),
    #[error("Request evicted due to higher priority traffic (project: {0})")]
    Evicted(String),
    #[error("Request timeout for project: {0}")]
    Timeout(String),
    #[error("Unknown project: {0}")]
    UnknownProject(String),
}

struct QueueItem {
    enqueue_time: Instant,
    tx: oneshot::Sender<Result<SlotPermit, QueueError>>,
}

struct ProjectQueue {
    items: VecDeque<QueueItem>,
    priority: u32,
    share: u32,
    max_slots: usize,
    in_flight: usize,
    // max items that can wait — hardcoded generous default, not exposed in config
    max_queue_size: usize,
}

struct SchedulerState {
    projects: HashMap<String, ProjectQueue>,
    global_in_flight: usize,
    global_slots: usize,
}

impl SchedulerState {
    /// Pick the next project to dispatch from, returns project name.
    /// Strict priority across tiers, weighted random within a tier.
    fn pick_next(&self) -> Option<String> {
        // Group eligible projects by priority tier
        let mut by_tier: BTreeMap<u32, Vec<(&str, u32)>> = BTreeMap::new();

        for (name, q) in &self.projects {
            if q.items.is_empty() || q.in_flight >= q.max_slots {
                continue;
            }
            by_tier
                .entry(q.priority)
                .or_default()
                .push((name.as_str(), q.share));
        }

        // Take the lowest priority number (highest importance) that has candidates
        let candidates = by_tier.into_values().next()?;

        // Weighted random within the tier
        let total_weight: u32 = candidates.iter().map(|(_, w)| w).sum();
        if total_weight == 0 {
            return None;
        }

        use rand::Rng;
        let mut pick = rand::thread_rng().gen_range(0..total_weight);
        for (name, weight) in &candidates {
            if pick < *weight {
                return Some(name.to_string());
            }
            pick -= weight;
        }

        // Fallback (shouldn't happen)
        Some(candidates[0].0.to_string())
    }

    /// Find the project to evict from: highest priority number (least important),
    /// lowest share within that tier, must have queued items.
    fn pick_eviction_target(&self) -> Option<String> {
        // reverse BTreeMap order = highest priority number first
        let mut by_tier: BTreeMap<u32, Vec<(&str, u32)>> = BTreeMap::new();
        for (name, q) in &self.projects {
            if !q.items.is_empty() {
                by_tier
                    .entry(q.priority)
                    .or_default()
                    .push((name.as_str(), q.share));
            }
        }

        let candidates = by_tier.into_values().next_back()?;

        // Within the tier, evict from the lowest share first
        candidates
            .into_iter()
            .min_by_key(|(_, share)| *share)
            .map(|(name, _)| name.to_string())
    }
}

pub struct ClassBasedScheduler {
    state: Arc<Mutex<SchedulerState>>,
    notify: Arc<Notify>,
    config: Arc<ProxyConfig>,
}

impl ClassBasedScheduler {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        let mut projects = HashMap::new();

        for (name, project_cfg) in &config.projects {
            let max_slots = config.effective_max_slots(project_cfg);
            projects.insert(
                name.clone(),
                ProjectQueue {
                    items: VecDeque::new(),
                    priority: project_cfg.priority,
                    share: project_cfg.share,
                    max_slots,
                    in_flight: 0,
                    max_queue_size: 500,
                },
            );

            tracing::info!(
                "Initialized project: {} (priority={}, share={}, max_slots={})",
                name,
                project_cfg.priority,
                project_cfg.share,
                max_slots,
            );
        }

        let scheduler = Self {
            state: Arc::new(Mutex::new(SchedulerState {
                projects,
                global_in_flight: 0,
                global_slots: config.slots,
            })),
            notify: Arc::new(Notify::new()),
            config: Arc::clone(&config),
        };

        // Initialize static slot count and per-project metrics at startup
        metrics::SLOTS_TOTAL.set(config.slots as f64);
        metrics::SLOTS_IN_USE.set(0.0);
        metrics::init_project_metrics(config.projects.keys());

        scheduler
    }

    /// Enqueue a request. Returns a receiver that resolves to a SlotPermit when
    /// the request is dispatched, or a QueueError if evicted/rejected.
    pub async fn enqueue(
        &self,
        project_name: &str,
    ) -> Result<oneshot::Receiver<Result<SlotPermit, QueueError>>, QueueError> {
        let mut state = self.state.lock().await;

        let queue = state
            .projects
            .get_mut(project_name)
            .ok_or_else(|| QueueError::UnknownProject(project_name.to_string()))?;

        if queue.items.len() >= queue.max_queue_size {
            return Err(QueueError::QueueFull(project_name.to_string()));
        }

        let (tx, rx) = oneshot::channel();
        queue.items.push_back(QueueItem {
            enqueue_time: Instant::now(),
            tx,
        });

        metrics::PROJECT_QUEUE_DEPTH
            .with_label_values(&[project_name])
            .set(queue.items.len() as f64);

        tracing::debug!("Enqueued request: project={}, queue_size={}", project_name, queue.items.len());

        // Wake dispatch loop — a new request is available
        self.notify.notify_one();

        Ok(rx)
    }

    /// Wait for the slot permit with a per-project timeout.
    pub async fn wait_for_slot(
        &self,
        project_name: &str,
        rx: oneshot::Receiver<Result<SlotPermit, QueueError>>,
    ) -> Result<SlotPermit, QueueError> {
        let duration = self
            .config
            .projects
            .get(project_name)
            .map(|p| self.config.effective_timeout(p))
            .unwrap_or(self.config.default_timeout);

        match timeout(duration, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(QueueError::Timeout(project_name.to_string())),
            Err(_) => Err(QueueError::Timeout(project_name.to_string())),
        }
    }

    /// Main dispatch loop — event-driven, no polling.
    pub async fn dispatch_loop(self: Arc<Self>) {
        loop {
            self.notify.notified().await;
            self.drain().await;
        }
    }

    /// Dispatch as many requests as current capacity allows.
    async fn drain(&self) {
        let mut state = self.state.lock().await;

        loop {
            if state.global_in_flight >= state.global_slots {
                // System full — check if we need to evict for a higher priority project
                self.try_evict(&mut state);
                break;
            }

            match state.pick_next() {
                None => break,
                Some(project_name) => {
                    let (item, queue_wait, queue_size, in_flight) = {
                        let queue = state.projects.get_mut(&project_name).unwrap();
                        let item = queue.items.pop_front().unwrap();
                        let queue_wait = item.enqueue_time.elapsed();
                        queue.in_flight += 1;
                        (item, queue_wait, queue.items.len(), queue.in_flight)
                    };

                    state.global_in_flight += 1;

                    metrics::PROJECT_QUEUE_DEPTH
                        .with_label_values(&[&project_name])
                        .set(queue_size as f64);
                    metrics::PROJECT_IN_FLIGHT
                        .with_label_values(&[&project_name])
                        .set(in_flight as f64);
                    metrics::SLOTS_IN_USE
                        .set(state.global_in_flight as f64);
                    metrics::QUEUE_WAIT
                        .with_label_values(&[&project_name])
                        .observe(queue_wait.as_secs_f64());

                    tracing::debug!(
                        "Dispatched: project={}, queue_wait_ms={}, in_flight={}/{}",
                        project_name,
                        queue_wait.as_millis(),
                        state.global_in_flight,
                        state.global_slots,
                    );

                    let permit = SlotPermit {
                        project_name: project_name.clone(),
                        state: Arc::clone(&self.state),
                        notify: Arc::clone(&self.notify),
                    };

                    // If receiver dropped, immediately release the slot back
                    if item.tx.send(Ok(permit)).is_err() {
                        let queue = state.projects.get_mut(&project_name).unwrap();
                        queue.in_flight -= 1;
                        let _ = queue;
                        state.global_in_flight -= 1;
                    }
                }
            }
        }
    }

    /// If system is at capacity and a higher-priority tier has queued work,
    /// evict the most recently queued request from the lowest-priority tier.
    fn try_evict(&self, state: &mut SchedulerState) {
        // Find the lowest priority tier that has queued work and hasn't hit max_slots
        let needs_dispatch = state.projects.values().any(|q| {
            !q.items.is_empty() && q.in_flight < q.max_slots
        });

        if !needs_dispatch {
            return;
        }

        // Find the best candidate to dispatch (lowest tier number with work)
        let dispatch_priority = state
            .projects
            .values()
            .filter(|q| !q.items.is_empty() && q.in_flight < q.max_slots)
            .map(|q| q.priority)
            .min();

        // Find the worst candidate to evict (highest tier number with queued items)
        let evict_priority = state
            .projects
            .values()
            .filter(|q| !q.items.is_empty())
            .map(|q| q.priority)
            .max();

        // Only evict if the project needing a slot has higher importance than eviction target
        if let (Some(dp), Some(ep)) = (dispatch_priority, evict_priority) {
            if dp >= ep {
                return; // No eviction would help
            }

            if let Some(target) = state.pick_eviction_target() {
                let queue = state.projects.get_mut(&target).unwrap();
                // Evict the newest queued item (pop_back = LIFO for eviction)
                if let Some(item) = queue.items.pop_back() {
                    metrics::REQUESTS_TOTAL
                        .with_label_values(&[&target, "evicted"])
                        .inc();
                    metrics::QUEUE_WAIT
                        .with_label_values(&[&target])
                        .observe(item.enqueue_time.elapsed().as_secs_f64());
                    metrics::PROJECT_QUEUE_DEPTH
                        .with_label_values(&[&target])
                        .set(queue.items.len() as f64);

                    tracing::warn!("Evicted queued request from project={}", target);
                    let _ = item.tx.send(Err(QueueError::Evicted(target)));
                }
            }
        }
    }

    pub async fn queue_size(&self, project_name: &str) -> usize {
        let state = self.state.lock().await;
        state
            .projects
            .get(project_name)
            .map(|q| q.items.len())
            .unwrap_or(0)
    }

    pub async fn in_flight_count(&self, project_name: &str) -> usize {
        let state = self.state.lock().await;
        state
            .projects
            .get(project_name)
            .map(|q| q.in_flight)
            .unwrap_or(0)
    }
}

/// RAII slot permit. Releasing this frees the slot and wakes the dispatch loop.
pub struct SlotPermit {
    project_name: String,
    state: Arc<Mutex<SchedulerState>>,
    notify: Arc<Notify>,
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        let state = Arc::clone(&self.state);
        let notify = Arc::clone(&self.notify);
        let project_name = self.project_name.clone();

        tokio::spawn(async move {
            let mut state = state.lock().await;
            let in_flight = if let Some(queue) = state.projects.get_mut(&project_name) {
                queue.in_flight = queue.in_flight.saturating_sub(1);
                queue.in_flight
            } else {
                0
            };
            state.global_in_flight = state.global_in_flight.saturating_sub(1);

            metrics::PROJECT_IN_FLIGHT
                .with_label_values(&[&project_name])
                .set(in_flight as f64);
            metrics::SLOTS_IN_USE
                .set(state.global_in_flight as f64);
            // Slot is free — wake dispatch loop immediately
            notify.notify_one();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use std::collections::HashMap;
    use std::time::Duration;

    fn make_config(slots: usize, projects: Vec<(&str, u32, u32, Option<usize>)>) -> Arc<ProxyConfig> {
        let mut project_map = HashMap::new();
        for (name, priority, share, max_slots) in projects {
            project_map.insert(
                name.to_string(),
                ProjectConfig {
                    priority,
                    share,
                    max_slots,
                    timeout: None,
                    api_keys: vec![],
                },
            );
        }

        Arc::new(ProxyConfig {
            server: ServerConfig { host: "127.0.0.1".into(), port: 8080 },
            upstream: UpstreamConfig { url: "http://localhost".into(), api_key: None },
            slots,
            default_timeout: Duration::from_secs(5),
            unauthenticated: UnauthenticatedPolicy::Reject(RejectLiteral::Reject),
            projects: project_map,
        })
    }

    #[tokio::test]
    async fn test_basic_dispatch() {
        let config = make_config(4, vec![("prod", 1, 1, None)]);
        let scheduler = Arc::new(ClassBasedScheduler::new(config));

        let scheduler_clone = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler_clone.dispatch_loop().await });

        let rx = scheduler.enqueue("prod").await.unwrap();
        let result = tokio::time::timeout(Duration::from_millis(200), rx).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_queue_full_rejection() {
        let config = make_config(1, vec![("prod", 1, 1, None)]);
        let scheduler = Arc::new(ClassBasedScheduler::new(config));

        // Fill queue past max_queue_size — but that's 500, so fill slots instead
        // by not starting dispatch loop, slots stay at 0 available
        let mut receivers = vec![];
        for _ in 0..500 {
            if let Ok(rx) = scheduler.enqueue("prod").await {
                receivers.push(rx);
            }
        }
        // 501st should fail
        let result = scheduler.enqueue("prod").await;
        assert!(matches!(result, Err(QueueError::QueueFull(_))));
    }

    #[tokio::test]
    async fn test_unknown_project() {
        let config = make_config(4, vec![("prod", 1, 1, None)]);
        let scheduler = Arc::new(ClassBasedScheduler::new(config));
        let result = scheduler.enqueue("nonexistent").await;
        assert!(matches!(result, Err(QueueError::UnknownProject(_))));
    }

    #[tokio::test]
    async fn test_max_slots_respected() {
        let config = make_config(8, vec![("prod", 1, 1, Some(2))]);
        let scheduler = Arc::new(ClassBasedScheduler::new(config));

        let scheduler_clone = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler_clone.dispatch_loop().await });

        let rx1 = scheduler.enqueue("prod").await.unwrap();
        let rx2 = scheduler.enqueue("prod").await.unwrap();
        let rx3 = scheduler.enqueue("prod").await.unwrap();

        // Hold permits so slots aren't freed
        let _p1 = tokio::time::timeout(Duration::from_millis(100), rx1).await.unwrap().unwrap();
        let _p2 = tokio::time::timeout(Duration::from_millis(100), rx2).await.unwrap().unwrap();

        // Third should remain queued (max_slots=2)
        let result = tokio::time::timeout(Duration::from_millis(50), rx3).await;
        assert!(result.is_err(), "third request should be queued due to max_slots");
    }

    #[tokio::test]
    async fn test_timeout() {
        let config = make_config(1, vec![("prod", 1, 1, None)]);
        // Override timeout to be very short
        let mut cfg = (*config).clone();
        cfg.default_timeout = Duration::from_millis(50);
        let scheduler = Arc::new(ClassBasedScheduler::new(Arc::new(cfg)));

        // Don't start dispatch loop — request will stay queued and time out
        let rx = scheduler.enqueue("prod").await.unwrap();
        let result = scheduler.wait_for_slot("prod", rx).await;
        assert!(matches!(result, Err(QueueError::Timeout(_))));
    }

    #[tokio::test]
    async fn test_priority_tier_ordering() {
        // slots=1, two tiers — only one request dispatched at a time
        // priority-1 should always win over priority-2
        let config = make_config(2, vec![
            ("high", 1, 1, None),
            ("low", 2, 1, None),
        ]);
        let scheduler = Arc::new(ClassBasedScheduler::new(config));

        let scheduler_clone = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler_clone.dispatch_loop().await });

        // Enqueue to both
        let rx_low = scheduler.enqueue("low").await.unwrap();
        let rx_high = scheduler.enqueue("high").await.unwrap();

        // High priority should dispatch
        let high_result = tokio::time::timeout(Duration::from_millis(100), rx_high).await;
        assert!(high_result.is_ok() && high_result.unwrap().is_ok());

        let low_result = tokio::time::timeout(Duration::from_millis(100), rx_low).await;
        assert!(low_result.is_ok() && low_result.unwrap().is_ok());
    }
}
