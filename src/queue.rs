use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, oneshot};
use thiserror::Error;

use crate::metrics;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("Queue is full and request priority is too low")]
    PriorityTooLow,
    #[error("Request was evicted from queue")]
    Evicted,
    #[error("Request timeout")]
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priority(pub i32);

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

struct QueueItem {
    priority: Priority,
    permit_tx: oneshot::Sender<Result<(), QueueError>>,
    // Sequence number for FIFO ordering within same priority
    sequence: u64,
}

impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl Eq for QueueItem {}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first, then earlier sequence (FIFO)
        match self.priority.cmp(&other.priority) {
            Ordering::Equal => other.sequence.cmp(&self.sequence),
            ord => ord,
        }
    }
}

pub struct PriorityQueue {
    heap: Arc<Mutex<BinaryHeap<QueueItem>>>,
    semaphore: Arc<Semaphore>,
    max_size: usize,
    sequence: Arc<Mutex<u64>>,
}

impl PriorityQueue {
    pub fn new(max_size: usize, max_concurrent: usize) -> Self {
        Self {
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_size,
            sequence: Arc::new(Mutex::new(0)),
        }
    }

    /// Enqueue a request with given priority
    /// Returns a receiver that will be notified when the request can proceed
    /// If queue is full and priority is high enough, evicts lowest priority item
    pub async fn enqueue(&self, priority: Priority) -> Result<oneshot::Receiver<Result<(), QueueError>>, QueueError> {
        let mut heap = self.heap.lock().await;
        
        // Get next sequence number
        let mut seq = self.sequence.lock().await;
        let sequence = *seq;
        *seq += 1;
        drop(seq);

        tracing::debug!(
            "Enqueue request: priority={}, sequence={}, current_queue_size={}, max_size={}",
            priority.0, sequence, heap.len(), self.max_size
        );

        // Check if queue is full
        if heap.len() >= self.max_size {
            tracing::warn!(
                "Queue full: checking eviction for priority={}, sequence={}",
                priority.0, sequence
            );
            
            // Get the lowest priority item (peek at the bottom of max heap)
            // Since BinaryHeap is a max heap, we need to find the minimum
            let min_priority = heap.iter().map(|item| item.priority).min();
            
            if let Some(min_priority) = min_priority {
                // If new request priority is higher or equal, evict the lowest priority item
                if priority >= min_priority {
                    tracing::info!(
                        "Evicting lowest priority request: new_priority={}, evicted_priority={}",
                        priority.0, min_priority.0
                    );
                    // Remove the lowest priority item
                    let mut items: Vec<_> = heap.drain().collect();
                    items.sort_by(|a, b| a.priority.cmp(&b.priority));
                    
                    let mut items_iter = items.into_iter();
                    if let Some(evicted) = items_iter.next() {
                        // Notify the evicted request
                        tracing::warn!(
                            "Request evicted: priority={}, sequence={}",
                            evicted.priority.0, evicted.sequence
                        );
                        
                        // Record eviction metrics
                        let evicted_priority = evicted.priority.0.to_string();
                        metrics::REQUESTS_EVICTED.with_label_values(&[&evicted_priority]).inc();
                        metrics::REQUESTS_OUTCOME.with_label_values(&["evicted"]).inc();
                        
                        let _ = evicted.permit_tx.send(Err(QueueError::Evicted));
                    }
                    
                    // Re-add remaining items
                    for item in items_iter {
                        heap.push(item);
                    }
                } else {
                    tracing::warn!(
                        "Request rejected: priority too low (priority={}, min_queue_priority={})",
                        priority.0, min_priority.0
                    );
                    
                    // Record rejection metrics
                    let priority_str = priority.0.to_string();
                    metrics::REQUESTS_REJECTED.with_label_values(&[&priority_str]).inc();
                    metrics::REQUESTS_OUTCOME.with_label_values(&["rejected"]).inc();
                    
                    return Err(QueueError::PriorityTooLow);
                }
            }
        }

        let (tx, rx) = oneshot::channel();
        
        heap.push(QueueItem {
            priority,
            permit_tx: tx,
            sequence,
        });

        let queue_size = heap.len();
        tracing::info!(
            "Request enqueued successfully: priority={}, sequence={}, new_queue_size={}",
            priority.0, sequence, queue_size
        );
        
        // Update queue size metric
        metrics::QUEUE_SIZE.set(queue_size as f64);

        Ok(rx)
    }

    /// Process the next item in the queue
    /// Should be called in a loop by worker tasks
    pub async fn process_next(&self) {
        let item = {
            let mut heap = self.heap.lock().await;
            let item = heap.pop();
            // Update queue size metric
            metrics::QUEUE_SIZE.set(heap.len() as f64);
            item
        };

        if let Some(item) = item {
            tracing::debug!(
                "Processing next request: priority={}, sequence={}",
                item.priority.0, item.sequence
            );
            
            // Acquire semaphore permit
            let permit = self.semaphore.acquire().await.unwrap();
            
            tracing::info!(
                "Request granted processing permit: priority={}, sequence={}",
                item.priority.0, item.sequence
            );
            
            // Notify the request it can proceed
            let _ = item.permit_tx.send(Ok(()));
            
            // The permit will be released when dropped
            // We need to keep it alive until the request completes
            // For now, we'll release it immediately, but in production
            // you'd want to pass this to the handler
            drop(permit);
        }
    }

    pub async fn queue_size(&self) -> usize {
        let heap = self.heap.lock().await;
        heap.len()
    }

    /// Update queue size distribution metrics
    pub async fn update_queue_distribution(&self) {
        let heap = self.heap.lock().await;
        
        // Reset all gauges to 0
        metrics::QUEUE_SIZE_BY_PRIORITY.reset();
        
        // Count items by priority
        for item in heap.iter() {
            let priority_str = item.priority.0.to_string();
            metrics::QUEUE_SIZE_BY_PRIORITY.with_label_values(&[&priority_str]).inc();
        }
    }

    /// Acquire a semaphore permit for processing
    pub async fn acquire_permit(&self) -> tokio::sync::SemaphorePermit<'_> {
        tracing::debug!("Acquiring semaphore permit for request processing");
        let permit = self.semaphore.acquire().await.unwrap();
        let available = self.semaphore.available_permits();
        tracing::debug!("Semaphore permit acquired, remaining permits={}", available);
        
        // Update available permits metric
        metrics::AVAILABLE_PERMITS.set(available as f64);
        
        permit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_priority_ordering() {
        let queue = PriorityQueue::new(10, 5);
        
        let _rx1 = queue.enqueue(Priority(1)).await.unwrap();
        let _rx2 = queue.enqueue(Priority(5)).await.unwrap();
        let _rx3 = queue.enqueue(Priority(3)).await.unwrap();
        
        assert_eq!(queue.queue_size().await, 3);
    }

    #[tokio::test]
    async fn test_eviction() {
        let queue = PriorityQueue::new(2, 5);
        
        let _rx1 = queue.enqueue(Priority(1)).await.unwrap();
        let _rx2 = queue.enqueue(Priority(2)).await.unwrap();
        
        // This should evict priority 1
        let _rx3 = queue.enqueue(Priority(3)).await.unwrap();
        
        assert_eq!(queue.queue_size().await, 2);
    }
}
