use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{oneshot, Mutex, Semaphore};
use tokio::time::timeout;

use crate::config::{SchedulerConfig, TrafficClassConfig};
use crate::metrics;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("Queue is full for traffic class {0}")]
    QueueFull(String),
    #[error("Request was evicted from queue due to higher priority traffic (class: {0})")]
    Evicted(String),
    #[error("Request timeout (total lifetime exceeded) for class {0}")]
    Timeout(String),
    #[error("Unknown traffic class: {0}")]
    UnknownClass(String),
}

/// Traffic class identifier (dynamic string-based)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrafficClass(pub String);

impl TrafficClass {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

struct QueueItem {
    enqueue_time: Instant,
    permit_tx: oneshot::Sender<Result<(), QueueError>>,
}

struct ClassQueue {
    items: VecDeque<QueueItem>,
    config: TrafficClassConfig,
    in_flight: usize,
    /// Eviction priority (higher = protected from eviction)
    priority: u32,
}

pub struct ClassBasedScheduler {
    queues: Arc<Mutex<HashMap<TrafficClass, ClassQueue>>>,
    global_semaphore: Arc<Semaphore>,
    timeout_duration: Duration,
}

impl ClassBasedScheduler {
    pub fn new(config: &SchedulerConfig) -> anyhow::Result<Self> {
        let mut queues = HashMap::new();

        if config.classes.is_empty() {
            anyhow::bail!("At least one traffic class must be configured");
        }

        // Initialize queues for each configured class
        for (class_name, class_config) in &config.classes {
            // Use explicit priority or derive from weight
            let priority = class_config.priority.unwrap_or(class_config.weight);

            queues.insert(
                TrafficClass::new(class_name.clone()),
                ClassQueue {
                    items: VecDeque::new(),
                    config: class_config.clone(),
                    in_flight: 0,
                    priority,
                },
            );

            tracing::info!(
                "Initialized traffic class: {} (weight={}, priority={}, min={}, max={})",
                class_name,
                class_config.weight,
                priority,
                class_config.min_concurrency,
                class_config.max_concurrency
            );
        }

        Ok(Self {
            queues: Arc::new(Mutex::new(queues)),
            global_semaphore: Arc::new(Semaphore::new(config.global_concurrency)),
            timeout_duration: Duration::from_secs(config.timeout_seconds),
        })
    }

    /// Enqueue a request for a traffic class
    /// Returns a receiver that will be notified when the request can proceed
    pub async fn enqueue(
        &self,
        class: TrafficClass,
    ) -> Result<oneshot::Receiver<Result<(), QueueError>>, QueueError> {
        let mut queues = self.queues.lock().await;
        let class_queue = queues
            .get_mut(&class)
            .ok_or_else(|| QueueError::UnknownClass(class.as_str().to_string()))?;

        let class_str = class.as_str();

        // Check if class queue is full
        if class_queue.items.len() >= class_queue.config.max_queue_size {
            tracing::warn!(
                "Queue full for class {}: queue_size={}, max={}",
                class_str,
                class_queue.items.len(),
                class_queue.config.max_queue_size
            );

            metrics::REQUESTS_REJECTED
                .with_label_values(&[class_str, "queue_full"])
                .inc();

            return Err(QueueError::QueueFull(class_str.to_string()));
        }

        let (tx, rx) = oneshot::channel();

        class_queue.items.push_back(QueueItem {
            enqueue_time: Instant::now(),
            permit_tx: tx,
        });

        let queue_size = class_queue.items.len();
        tracing::debug!(
            "Request enqueued: class={}, queue_size={}",
            class_str,
            queue_size
        );

        // Update metrics
        metrics::QUEUE_SIZE_BY_CLASS
            .with_label_values(&[class_str])
            .set(queue_size as f64);

        Ok(rx)
    }

    /// Dispatch loop - continuously dispatches requests using weighted fair scheduling
    pub async fn dispatch_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Try to dispatch a request
            if let Err(e) = self.try_dispatch().await {
                tracing::trace!("Dispatch attempt: {}", e);
            }
        }
    }

    /// Try to dispatch one request using weighted fair scheduling
    async fn try_dispatch(&self) -> anyhow::Result<()> {
        let mut queues = self.queues.lock().await;

        // Check if we have global capacity
        let available_permits = self.global_semaphore.available_permits();
        if available_permits == 0 {
            return Err(anyhow::anyhow!("No global capacity available"));
        }

        // Update available permits metric
        metrics::AVAILABLE_PERMITS.set(available_permits as f64);

        // Collect classes that can accept more in-flight requests
        let candidates: Vec<(TrafficClass, u32, usize)> = queues
            .iter()
            .filter(|(class, queue)| {
                !queue.items.is_empty()
                    && queue.in_flight < queue.config.max_concurrency
                    && self.check_min_concurrency_satisfied(&queues, class, queue)
            })
            .map(|(class, queue)| (class.clone(), queue.config.weight, queue.items.len()))
            .collect();

        if candidates.is_empty() {
            // Check if we need to evict to make room for min_concurrency
            self.check_eviction(&mut queues).await;
            return Err(anyhow::anyhow!("No dispatchable requests"));
        }

        // Weighted fair selection
        let total_weight: u32 = candidates.iter().map(|(_, w, _)| w).sum();
        let selected_class = self.weighted_select(&candidates, total_weight);

        // Dispatch from selected class
        if let Some(class_queue) = queues.get_mut(&selected_class) {
            if let Some(item) = class_queue.items.pop_front() {
                let class_str = selected_class.as_str();
                let queue_wait = item.enqueue_time.elapsed();

                tracing::debug!(
                    "Dispatching request: class={}, queue_wait_ms={}",
                    class_str,
                    queue_wait.as_millis()
                );

                // Update metrics
                metrics::QUEUE_SIZE_BY_CLASS
                    .with_label_values(&[class_str])
                    .set(class_queue.items.len() as f64);

                metrics::QUEUE_DURATION
                    .with_label_values(&[class_str])
                    .observe(queue_wait.as_secs_f64());

                class_queue.in_flight += 1;
                metrics::IN_FLIGHT_REQUESTS
                    .with_label_values(&[class_str])
                    .set(class_queue.in_flight as f64);

                // Notify the request it can proceed
                let _ = item.permit_tx.send(Ok(()));
            }
        }

        Ok(())
    }

    /// Check if min_concurrency requirements are satisfied for higher priority classes
    fn check_min_concurrency_satisfied(
        &self,
        queues: &HashMap<TrafficClass, ClassQueue>,
        _candidate_class: &TrafficClass,
        candidate_queue: &ClassQueue,
    ) -> bool {
        // If this class hasn't met its min, allow it
        if candidate_queue.in_flight < candidate_queue.config.min_concurrency {
            return true;
        }

        // Check if higher priority classes have unmet minimums
        for (_class, queue) in queues {
            if queue.priority > candidate_queue.priority
                && !queue.items.is_empty()
                && queue.in_flight < queue.config.min_concurrency
            {
                // Higher priority class needs capacity
                return false;
            }
        }

        true
    }

    /// Check if we should evict requests from lower priority classes
    async fn check_eviction(&self, queues: &mut HashMap<TrafficClass, ClassQueue>) {
        let global_in_flight: usize = queues.values().map(|q| q.in_flight).sum();
        let global_capacity = self.global_semaphore.available_permits() + global_in_flight;

        // Collect classes that need eviction
        let mut needs_eviction = Vec::new();
        for (class, queue) in queues.iter() {
            if !queue.items.is_empty()
                && queue.in_flight < queue.config.min_concurrency
                && global_in_flight >= global_capacity
            {
                needs_eviction.push(class.clone());
            }
        }

        // Process evictions
        for class in needs_eviction {
            self.evict_from_lower_priority(queues, class).await;
        }
    }

    /// Evict queued requests from lower priority classes
    async fn evict_from_lower_priority(
        &self,
        queues: &mut HashMap<TrafficClass, ClassQueue>,
        target_class: TrafficClass,
    ) {
        // Get target priority
        let target_priority = queues.get(&target_class).map(|q| q.priority).unwrap_or(0);

        // Sort classes by priority (lowest first)
        let mut classes_by_priority: Vec<(TrafficClass, u32)> = queues
            .iter()
            .map(|(class, queue)| (class.clone(), queue.priority))
            .collect();
        classes_by_priority.sort_by_key(|(_, priority)| *priority);

        for (class, priority) in classes_by_priority {
            if priority >= target_priority {
                break;
            }

            if let Some(queue) = queues.get_mut(&class) {
                if let Some(item) = queue.items.pop_back() {
                    let class_str = class.as_str();
                    tracing::warn!("Evicting request from class {}", class_str);

                    metrics::REQUESTS_EVICTED
                        .with_label_values(&[class_str])
                        .inc();

                    metrics::QUEUE_SIZE_BY_CLASS
                        .with_label_values(&[class_str])
                        .set(queue.items.len() as f64);

                    let _ = item
                        .permit_tx
                        .send(Err(QueueError::Evicted(class_str.to_string())));
                    return;
                }
            }
        }
    }

    /// Weighted random selection
    fn weighted_select(
        &self,
        candidates: &[(TrafficClass, u32, usize)],
        total_weight: u32,
    ) -> TrafficClass {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut selection = rng.gen_range(0..total_weight);

        for (class, weight, _) in candidates {
            if selection < *weight {
                return class.clone();
            }
            selection -= weight;
        }

        // Fallback
        candidates[0].0.clone()
    }

    /// Acquire a permit and track it for a specific class
    pub async fn acquire_permit(self: &Arc<Self>, class: TrafficClass) -> SchedulerPermit {
        let permit = Arc::clone(&self.global_semaphore)
            .acquire_owned()
            .await
            .unwrap();

        SchedulerPermit {
            class,
            permit: Some(permit),
            queues: self.queues.clone(),
        }
    }

    /// Wait for a request with timeout
    pub async fn wait_with_timeout(
        &self,
        class: TrafficClass,
        rx: oneshot::Receiver<Result<(), QueueError>>,
    ) -> Result<(), QueueError> {
        match timeout(self.timeout_duration, rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(QueueError::Timeout(class.as_str().to_string())),
            Err(_) => Err(QueueError::Timeout(class.as_str().to_string())),
        }
    }

    /// Get current queue size for a class
    pub async fn queue_size(&self, class: TrafficClass) -> usize {
        let queues = self.queues.lock().await;
        queues.get(&class).map(|q| q.items.len()).unwrap_or(0)
    }

    /// Get in-flight count for a class
    pub async fn in_flight_count(&self, class: TrafficClass) -> usize {
        let queues = self.queues.lock().await;
        queues.get(&class).map(|q| q.in_flight).unwrap_or(0)
    }
}

/// RAII permit that updates metrics on drop
pub struct SchedulerPermit {
    class: TrafficClass,
    #[allow(dead_code)]
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    queues: Arc<Mutex<HashMap<TrafficClass, ClassQueue>>>,
}

impl Drop for SchedulerPermit {
    fn drop(&mut self) {
        let class = self.class.clone();
        let queues = self.queues.clone();

        // Spawn a task to update metrics
        tokio::spawn(async move {
            let mut queues = queues.lock().await;
            if let Some(queue) = queues.get_mut(&class) {
                queue.in_flight = queue.in_flight.saturating_sub(1);

                let class_str = class.as_str();
                metrics::IN_FLIGHT_REQUESTS
                    .with_label_values(&[class_str])
                    .set(queue.in_flight as f64);

                tracing::debug!(
                    "Request completed: class={}, in_flight={}",
                    class_str,
                    queue.in_flight
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SchedulerConfig {
        let mut classes = HashMap::new();
        classes.insert(
            "high".to_string(),
            TrafficClassConfig {
                weight: 10,
                priority: Some(100),
                min_concurrency: 3,
                max_concurrency: 8,
                max_queue_size: 50,
            },
        );
        classes.insert(
            "normal".to_string(),
            TrafficClassConfig {
                weight: 5,
                priority: Some(50),
                min_concurrency: 1,
                max_concurrency: 5,
                max_queue_size: 30,
            },
        );
        classes.insert(
            "low".to_string(),
            TrafficClassConfig {
                weight: 2,
                priority: Some(10),
                min_concurrency: 0,
                max_concurrency: 3,
                max_queue_size: 20,
            },
        );

        SchedulerConfig {
            global_concurrency: 10,
            timeout_seconds: 1, // Short timeout for tests
            classes,
        }
    }

    #[tokio::test]
    async fn test_scheduler_initialization() {
        let config = test_config();
        let result = ClassBasedScheduler::new(&config);
        assert!(result.is_ok());

        let scheduler = result.unwrap();
        let queues = scheduler.queues.lock().await;
        assert_eq!(queues.len(), 3);
        assert!(queues.contains_key(&TrafficClass::new("high")));
        assert!(queues.contains_key(&TrafficClass::new("normal")));
        assert!(queues.contains_key(&TrafficClass::new("low")));
    }

    #[tokio::test]
    async fn test_scheduler_requires_classes() {
        let config = SchedulerConfig {
            global_concurrency: 10,
            timeout_seconds: 300,
            classes: HashMap::new(),
        };

        let result = ClassBasedScheduler::new(&config);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.err().unwrap());
        assert!(error_msg.contains("At least one traffic class"));
    }

    #[tokio::test]
    async fn test_enqueue_and_dispatch() {
        let config = test_config();
        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        // Start dispatcher
        let scheduler_clone = scheduler.clone();
        tokio::spawn(async move {
            scheduler_clone.dispatch_loop().await;
        });

        // Enqueue a request
        let rx = scheduler.enqueue(TrafficClass::new("high")).await.unwrap();

        // Wait for dispatch with timeout
        let result = tokio::time::timeout(Duration::from_millis(100), rx).await;
        assert!(result.is_ok(), "Request should be dispatched");
        assert!(result.unwrap().is_ok(), "Dispatch should succeed");
    }

    #[tokio::test]
    async fn test_queue_full_rejection() {
        let mut config = test_config();
        config.classes.get_mut("low").unwrap().max_queue_size = 2;

        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        // Fill the queue
        let _rx1 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();
        let _rx2 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();

        // Third request should be rejected
        let result = scheduler.enqueue(TrafficClass::new("low")).await;
        assert!(matches!(result, Err(QueueError::QueueFull(_))));
    }

    #[tokio::test]
    async fn test_unknown_class_rejection() {
        let config = test_config();
        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        let result = scheduler.enqueue(TrafficClass::new("nonexistent")).await;
        assert!(matches!(result, Err(QueueError::UnknownClass(_))));
    }

    #[tokio::test]
    async fn test_max_concurrency_limit() {
        let mut config = test_config();
        config.classes.get_mut("low").unwrap().max_concurrency = 2;
        config.global_concurrency = 10; // Plenty of global capacity

        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        // Start dispatcher
        let scheduler_clone = scheduler.clone();
        tokio::spawn(async move {
            scheduler_clone.dispatch_loop().await;
        });

        // Enqueue requests and acquire permits
        let rx1 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();
        let rx2 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();
        let rx3 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();

        // First two should dispatch
        let _ = tokio::time::timeout(Duration::from_millis(100), rx1)
            .await
            .unwrap()
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(100), rx2)
            .await
            .unwrap()
            .unwrap();

        // Third should still be queued (max_concurrency = 2)
        let result = tokio::time::timeout(Duration::from_millis(50), rx3).await;
        assert!(
            result.is_err(),
            "Third request should remain queued due to max_concurrency"
        );
    }

    #[tokio::test]
    async fn test_request_timeout() {
        let mut config = test_config();
        config.timeout_seconds = 0; // Immediate timeout

        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        // Don't start dispatcher so request stays queued
        let rx = scheduler.enqueue(TrafficClass::new("high")).await.unwrap();

        let result = scheduler
            .wait_with_timeout(TrafficClass::new("high"), rx)
            .await;
        assert!(matches!(result, Err(QueueError::Timeout(_))));
    }

    #[tokio::test]
    async fn test_queue_metrics() {
        let config = test_config();
        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        let class = TrafficClass::new("high");

        // Initially empty
        assert_eq!(scheduler.queue_size(class.clone()).await, 0);
        assert_eq!(scheduler.in_flight_count(class.clone()).await, 0);

        // Enqueue some requests
        let _rx1 = scheduler.enqueue(class.clone()).await.unwrap();
        let _rx2 = scheduler.enqueue(class.clone()).await.unwrap();

        assert_eq!(scheduler.queue_size(class.clone()).await, 2);
        assert_eq!(scheduler.in_flight_count(class.clone()).await, 0);
    }

    #[tokio::test]
    async fn test_weighted_selection_favors_higher_weight() {
        let mut config = test_config();
        // Make weights very different and increase queue sizes
        config.classes.get_mut("high").unwrap().weight = 100;
        config.classes.get_mut("high").unwrap().max_queue_size = 200;
        config.classes.get_mut("low").unwrap().weight = 1;
        config.classes.get_mut("low").unwrap().max_queue_size = 200;

        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        // Start dispatcher
        let scheduler_clone = scheduler.clone();
        tokio::spawn(async move {
            scheduler_clone.dispatch_loop().await;
        });

        // Enqueue many requests from both classes
        let mut high_dispatched = 0;
        let mut low_dispatched = 0;

        for _ in 0..30 {
            // Try to enqueue, but don't fail if queue is full
            if let Ok(rx_high) = scheduler.enqueue(TrafficClass::new("high")).await {
                if tokio::time::timeout(Duration::from_millis(100), rx_high)
                    .await
                    .is_ok()
                {
                    high_dispatched += 1;
                }
            }
            if let Ok(rx_low) = scheduler.enqueue(TrafficClass::new("low")).await {
                if tokio::time::timeout(Duration::from_millis(100), rx_low)
                    .await
                    .is_ok()
                {
                    low_dispatched += 1;
                }
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // High weight class should get significantly more dispatches
        // With 100:1 weight ratio, we expect roughly 100x more dispatches
        // Being conservative: at least 2x more (accounting for test timing variance)
        assert!(
            high_dispatched > low_dispatched * 2,
            "High priority (weight 100) got {} dispatches, low priority (weight 1) got {}",
            high_dispatched,
            low_dispatched
        );
    }

    #[tokio::test]
    async fn test_priority_defaults_to_weight() {
        let mut classes = HashMap::new();
        classes.insert(
            "test".to_string(),
            TrafficClassConfig {
                weight: 42,
                priority: None, // Not specified
                min_concurrency: 1,
                max_concurrency: 5,
                max_queue_size: 10,
            },
        );

        let config = SchedulerConfig {
            global_concurrency: 10,
            timeout_seconds: 300,
            classes,
        };

        let scheduler = ClassBasedScheduler::new(&config).unwrap();
        let queues = scheduler.queues.lock().await;
        let test_queue = queues.get(&TrafficClass::new("test")).unwrap();

        assert_eq!(test_queue.priority, 42, "Priority should default to weight");
    }

    #[tokio::test]
    async fn test_traffic_class_creation() {
        let class1 = TrafficClass::new("production");
        let class2 = TrafficClass::new("production".to_string());
        let class3 = TrafficClass::new("staging");

        assert_eq!(class1.as_str(), "production");
        assert_eq!(class1, class2);
        assert_ne!(class1, class3);
    }

    #[tokio::test]
    async fn test_eviction_under_pressure() {
        let mut config = test_config();
        config.global_concurrency = 3; // Very limited
        config.classes.get_mut("high").unwrap().min_concurrency = 2;
        config.classes.get_mut("low").unwrap().min_concurrency = 0;

        let scheduler = Arc::new(ClassBasedScheduler::new(&config).unwrap());

        // Start dispatcher
        let scheduler_clone = scheduler.clone();
        tokio::spawn(async move {
            scheduler_clone.dispatch_loop().await;
        });

        // Fill capacity with low priority requests
        let _low_rx1 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();
        let _low_rx2 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();
        let _low_rx3 = scheduler.enqueue(TrafficClass::new("low")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now enqueue high priority requests that need min_concurrency
        let high_rx1 = scheduler.enqueue(TrafficClass::new("high")).await.unwrap();
        let _high_rx2 = scheduler.enqueue(TrafficClass::new("high")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // At least one low priority request should be evicted or queued
        // High priority should eventually get its requests dispatched
        let high1_result = tokio::time::timeout(Duration::from_millis(50), high_rx1).await;
        assert!(
            high1_result.is_ok() || scheduler.queue_size(TrafficClass::new("high")).await == 0,
            "High priority requests should eventually be processed"
        );
    }
}
