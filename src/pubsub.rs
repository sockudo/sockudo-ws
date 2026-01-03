//! High-performance pub/sub system for WebSocket connections
//!
//! This module provides an ultra-fast topic-based publish/subscribe system
//! inspired by uWebSockets, designed for maximum throughput with:
//!
//! - **Sharded topics**: Reduces lock contention across topics
//! - **Lock-free subscriber IDs**: Atomic allocation of subscriber identifiers
//! - **Zero-copy messages**: Uses `Bytes` for efficient message sharing
//! - **Cache-line alignment**: Prevents false sharing in concurrent access
//! - **Batched delivery**: Amortizes overhead across multiple subscribers
//! - **Pusher-style string IDs**: Optional string-based subscriber identifiers
//!
//! # Example (Numeric IDs - High Performance)
//!
//! ```ignore
//! use sockudo_ws::pubsub::{PubSub, SubscriberId};
//! use sockudo_ws::Message;
//! use tokio::sync::mpsc;
//!
//! // Create pub/sub system
//! let pubsub = PubSub::new();
//!
//! // Create a subscriber with a message channel
//! let (tx, mut rx) = mpsc::unbounded_channel();
//! let sub_id = pubsub.create_subscriber(tx);
//!
//! // Subscribe to topics
//! pubsub.subscribe(sub_id, "chat");
//! pubsub.subscribe(sub_id, "notifications");
//!
//! // Publish messages
//! pubsub.publish("chat", Message::text("Hello, world!"));
//!
//! // Publish excluding the sender (common pattern)
//! pubsub.publish_excluding(sub_id, "chat", Message::text("Broadcast from me"));
//!
//! // Cleanup
//! pubsub.remove_subscriber(sub_id);
//! ```
//!
//! # Example (Pusher-style String IDs)
//!
//! ```ignore
//! use sockudo_ws::pubsub::PubSub;
//! use sockudo_ws::Message;
//! use tokio::sync::mpsc;
//!
//! let pubsub = PubSub::new();
//!
//! // Create subscriber with Pusher-style socket ID
//! let (tx, mut rx) = mpsc::unbounded_channel();
//! let socket_id = "1234.5678"; // Pusher format: random.random
//! let sub_id = pubsub.create_subscriber_with_id(socket_id, tx);
//!
//! // Subscribe using string ID
//! pubsub.subscribe_by_socket_id(socket_id, "private-chat");
//!
//! // Publish excluding by socket ID
//! pubsub.publish_excluding_socket_id(socket_id, "private-chat", Message::text("Hello"));
//!
//! // Lookup subscriber ID from socket ID
//! if let Some(id) = pubsub.get_subscriber_by_socket_id(socket_id) {
//!     println!("Found subscriber: {:?}", id);
//! }
//!
//! // Remove by socket ID
//! pubsub.remove_subscriber_by_socket_id(socket_id);
//! ```

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::RwLock;
use tokio::sync::mpsc::UnboundedSender;

use crate::CACHE_LINE_SIZE;
use crate::protocol::Message;

/// Number of shards for topic partitioning (power of 2 for fast modulo)
const NUM_SHARDS: usize = 64;

/// Unique identifier for a subscriber
///
/// Subscribers are identified by a dense, atomically-allocated ID.
/// This allows O(1) lookup and efficient exclusion during publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberId(pub u64);

impl SubscriberId {
    /// Get the raw ID value
    #[inline]
    pub fn as_u64(&self) -> u64 {
        self.0
    }

    /// Create a SubscriberId from a raw u64 value
    #[inline]
    pub fn from_u64(id: u64) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for SubscriberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Result of a publish operation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishResult {
    /// Message was published to N subscribers
    Published(usize),
    /// Topic does not exist (no subscribers)
    NoSubscribers,
}

impl PublishResult {
    /// Get the number of subscribers that received the message
    #[inline]
    pub fn count(&self) -> usize {
        match self {
            PublishResult::Published(n) => *n,
            PublishResult::NoSubscribers => 0,
        }
    }
}

/// A subscriber with its message channel
struct Subscriber {
    /// Channel for sending messages to this subscriber
    sender: UnboundedSender<Message>,
    /// Topics this subscriber is subscribed to (for cleanup)
    topics: RwLock<HashSet<String>>,
    /// Optional Pusher-style socket ID (e.g., "1234.5678")
    socket_id: Option<String>,
}

/// A topic with its subscribers
#[derive(Default)]
struct Topic {
    /// Set of subscriber IDs subscribed to this topic
    subscribers: HashSet<SubscriberId>,
}

/// Cache-line aligned shard for topic storage
#[repr(C, align(64))]
struct TopicShard {
    /// Topics in this shard
    topics: RwLock<HashMap<String, Topic>>,
    /// Padding to fill cache line
    _padding: [u8; CACHE_LINE_SIZE - std::mem::size_of::<RwLock<HashMap<String, Topic>>>()],
}

impl TopicShard {
    fn new() -> Self {
        Self {
            topics: RwLock::new(HashMap::new()),
            _padding: [0; CACHE_LINE_SIZE - std::mem::size_of::<RwLock<HashMap<String, Topic>>>()],
        }
    }
}

/// High-performance hasher for topic names (FxHash-style)
#[derive(Default)]
struct TopicHasher {
    hash: u64,
}

impl Hasher for TopicHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // FxHash-style mixing for fast string hashing
        const K: u64 = 0x517cc1b727220a95;
        for byte in bytes {
            self.hash = (self.hash.rotate_left(5) ^ (*byte as u64)).wrapping_mul(K);
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[derive(Default)]
struct TopicHasherBuilder;

impl BuildHasher for TopicHasherBuilder {
    type Hasher = TopicHasher;

    #[inline]
    fn build_hasher(&self) -> TopicHasher {
        TopicHasher::default()
    }
}

/// High-performance pub/sub state
///
/// This is the main entry point for the pub/sub system. It manages
/// topics, subscribers, and message delivery with minimal locking.
///
/// # Thread Safety
///
/// `PubSub` is fully thread-safe and can be shared across async tasks.
/// It uses sharding to minimize contention when different topics are
/// accessed concurrently.
///
/// # Subscriber ID Modes
///
/// The pub/sub system supports two modes for subscriber identification:
///
/// 1. **Numeric IDs (default)**: Auto-generated u64 IDs for maximum performance.
///    Use `create_subscriber()` for this mode.
///
/// 2. **Pusher-style String IDs**: Custom string identifiers like "1234.5678".
///    Use `create_subscriber_with_id()` for this mode. String IDs are mapped
///    to internal numeric IDs for efficient lookup.
pub struct PubSub {
    /// Topic shards for reduced contention
    shards: Box<[TopicShard; NUM_SHARDS]>,
    /// All subscribers indexed by ID
    subscribers: RwLock<HashMap<SubscriberId, Arc<Subscriber>>>,
    /// Socket ID to SubscriberId mapping (for Pusher-style IDs)
    socket_id_map: RwLock<HashMap<String, SubscriberId>>,
    /// Next subscriber ID (atomic counter)
    next_subscriber_id: AtomicU64,
    /// Total number of active subscribers
    subscriber_count: AtomicUsize,
    /// Total number of topics with at least one subscriber
    topic_count: AtomicUsize,
    /// Total messages published (for stats)
    messages_published: AtomicU64,
}

/// Type alias for backward compatibility
pub type PubSubState = PubSub;

impl PubSub {
    /// Create a new pub/sub system
    pub fn new() -> Self {
        // Initialize shards
        let shards: Box<[TopicShard; NUM_SHARDS]> = (0..NUM_SHARDS)
            .map(|_| TopicShard::new())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap_or_else(|_| panic!("Failed to create topic shards"));

        Self {
            shards,
            subscribers: RwLock::new(HashMap::new()),
            socket_id_map: RwLock::new(HashMap::new()),
            next_subscriber_id: AtomicU64::new(1),
            subscriber_count: AtomicUsize::new(0),
            topic_count: AtomicUsize::new(0),
            messages_published: AtomicU64::new(0),
        }
    }

    /// Get the shard index for a topic
    #[inline]
    fn shard_index(&self, topic: &str) -> usize {
        let mut hasher = TopicHasher::default();
        topic.hash(&mut hasher);
        (hasher.finish() as usize) & (NUM_SHARDS - 1)
    }

    /// Get a reference to a topic shard
    #[inline]
    fn shard(&self, topic: &str) -> &TopicShard {
        &self.shards[self.shard_index(topic)]
    }

    // =========================================================================
    // Subscriber Management (Numeric IDs)
    // =========================================================================

    /// Create a new subscriber and return its ID
    ///
    /// The subscriber will receive messages on the provided channel.
    ///
    /// # Arguments
    ///
    /// * `sender` - Unbounded channel sender for delivering messages
    ///
    /// # Returns
    ///
    /// A unique `SubscriberId` for this subscriber
    pub fn create_subscriber(&self, sender: UnboundedSender<Message>) -> SubscriberId {
        let id = SubscriberId(self.next_subscriber_id.fetch_add(1, Ordering::Relaxed));

        let subscriber = Arc::new(Subscriber {
            sender,
            topics: RwLock::new(HashSet::new()),
            socket_id: None,
        });

        self.subscribers.write().insert(id, subscriber);
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);

        id
    }

    /// Remove a subscriber and unsubscribe from all topics
    ///
    /// This should be called when a WebSocket connection closes.
    pub fn remove_subscriber(&self, id: SubscriberId) {
        // Get the subscriber and its topics
        let subscriber = {
            let mut subs = self.subscribers.write();
            subs.remove(&id)
        };

        if let Some(sub) = subscriber {
            // Remove from socket_id_map if present
            if let Some(ref socket_id) = sub.socket_id {
                self.socket_id_map.write().remove(socket_id);
            }

            // Unsubscribe from all topics
            let topics: Vec<String> = sub.topics.read().iter().cloned().collect();
            for topic in topics {
                self.unsubscribe_internal(id, &topic);
            }
            self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    // =========================================================================
    // Pusher-style String ID Support
    // =========================================================================

    /// Generate a Pusher-style socket ID
    ///
    /// Format: `{random}.{random}` where each part is a random number.
    /// Example: "1234567890.9876543210"
    pub fn generate_socket_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        // Use time-based pseudo-random values
        let part1 = (now & 0xFFFFFFFFFF) ^ (now >> 40);
        let part2 = (now >> 20) ^ (now & 0xFFFFF);

        format!("{}.{}", part1, part2)
    }

    /// Create a new subscriber with a custom Pusher-style socket ID
    ///
    /// This allows using string identifiers like "1234.5678" instead of
    /// numeric IDs. The string ID is mapped internally to a numeric ID
    /// for efficient operations.
    ///
    /// # Arguments
    ///
    /// * `socket_id` - Custom string identifier (e.g., "1234.5678")
    /// * `sender` - Unbounded channel sender for delivering messages
    ///
    /// # Returns
    ///
    /// A unique `SubscriberId` for this subscriber
    ///
    /// # Panics
    ///
    /// Panics if the socket_id is already in use. Use `get_subscriber_by_socket_id`
    /// to check first, or use `create_subscriber_with_id_or_get` for idempotent creation.
    pub fn create_subscriber_with_id(
        &self,
        socket_id: &str,
        sender: UnboundedSender<Message>,
    ) -> SubscriberId {
        let id = SubscriberId(self.next_subscriber_id.fetch_add(1, Ordering::Relaxed));

        let subscriber = Arc::new(Subscriber {
            sender,
            topics: RwLock::new(HashSet::new()),
            socket_id: Some(socket_id.to_string()),
        });

        // Insert into socket_id_map
        {
            let mut map = self.socket_id_map.write();
            if map.contains_key(socket_id) {
                panic!("Socket ID '{}' is already in use", socket_id);
            }
            map.insert(socket_id.to_string(), id);
        }

        self.subscribers.write().insert(id, subscriber);
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);

        id
    }

    /// Create a subscriber with a socket ID, or return existing one if already registered
    ///
    /// This is an idempotent version of `create_subscriber_with_id`.
    ///
    /// # Returns
    ///
    /// A tuple of (SubscriberId, bool) where the bool indicates if a new
    /// subscriber was created (true) or an existing one was returned (false).
    pub fn create_subscriber_with_id_or_get(
        &self,
        socket_id: &str,
        sender: UnboundedSender<Message>,
    ) -> (SubscriberId, bool) {
        // Check if already exists
        {
            let map = self.socket_id_map.read();
            if let Some(&id) = map.get(socket_id) {
                return (id, false);
            }
        }

        // Create new subscriber
        let id = SubscriberId(self.next_subscriber_id.fetch_add(1, Ordering::Relaxed));

        let subscriber = Arc::new(Subscriber {
            sender,
            topics: RwLock::new(HashSet::new()),
            socket_id: Some(socket_id.to_string()),
        });

        // Double-check and insert
        {
            let mut map = self.socket_id_map.write();
            if let Some(&existing_id) = map.get(socket_id) {
                // Race condition: someone else created it
                return (existing_id, false);
            }
            map.insert(socket_id.to_string(), id);
        }

        self.subscribers.write().insert(id, subscriber);
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);

        (id, true)
    }

    /// Get a subscriber ID by its socket ID
    ///
    /// # Arguments
    ///
    /// * `socket_id` - The Pusher-style socket ID
    ///
    /// # Returns
    ///
    /// The `SubscriberId` if found, or `None`
    pub fn get_subscriber_by_socket_id(&self, socket_id: &str) -> Option<SubscriberId> {
        self.socket_id_map.read().get(socket_id).copied()
    }

    /// Get the socket ID for a subscriber
    ///
    /// # Arguments
    ///
    /// * `id` - The subscriber ID
    ///
    /// # Returns
    ///
    /// The socket ID if the subscriber has one, or `None`
    pub fn get_socket_id(&self, id: SubscriberId) -> Option<String> {
        self.subscribers
            .read()
            .get(&id)
            .and_then(|sub| sub.socket_id.clone())
    }

    /// Remove a subscriber by its socket ID
    ///
    /// # Arguments
    ///
    /// * `socket_id` - The Pusher-style socket ID
    ///
    /// # Returns
    ///
    /// `true` if the subscriber was removed, `false` if not found
    pub fn remove_subscriber_by_socket_id(&self, socket_id: &str) -> bool {
        if let Some(id) = self.get_subscriber_by_socket_id(socket_id) {
            self.remove_subscriber(id);
            true
        } else {
            false
        }
    }

    /// Subscribe to a topic using socket ID
    ///
    /// # Arguments
    ///
    /// * `socket_id` - The Pusher-style socket ID
    /// * `topic` - The topic name to subscribe to
    ///
    /// # Returns
    ///
    /// `true` if newly subscribed, `false` if already subscribed or subscriber not found
    pub fn subscribe_by_socket_id(&self, socket_id: &str, topic: &str) -> bool {
        if let Some(id) = self.get_subscriber_by_socket_id(socket_id) {
            self.subscribe(id, topic)
        } else {
            false
        }
    }

    /// Unsubscribe from a topic using socket ID
    ///
    /// # Arguments
    ///
    /// * `socket_id` - The Pusher-style socket ID
    /// * `topic` - The topic name to unsubscribe from
    ///
    /// # Returns
    ///
    /// `true` if was subscribed, `false` if wasn't subscribed or subscriber not found
    pub fn unsubscribe_by_socket_id(&self, socket_id: &str, topic: &str) -> bool {
        if let Some(id) = self.get_subscriber_by_socket_id(socket_id) {
            self.unsubscribe(id, topic)
        } else {
            false
        }
    }

    /// Publish a message excluding a subscriber by socket ID
    ///
    /// # Arguments
    ///
    /// * `socket_id` - The socket ID to exclude
    /// * `topic` - The topic to publish to
    /// * `message` - The message to publish
    ///
    /// # Returns
    ///
    /// Result indicating how many subscribers received the message
    pub fn publish_excluding_socket_id(
        &self,
        socket_id: &str,
        topic: &str,
        message: Message,
    ) -> PublishResult {
        if let Some(id) = self.get_subscriber_by_socket_id(socket_id) {
            self.publish_excluding(id, topic, message)
        } else {
            // Socket ID not found, publish to all
            self.publish(topic, message)
        }
    }

    /// Check if a socket ID is subscribed to a topic
    pub fn is_subscribed_by_socket_id(&self, socket_id: &str, topic: &str) -> bool {
        if let Some(id) = self.get_subscriber_by_socket_id(socket_id) {
            self.is_subscribed(id, topic)
        } else {
            false
        }
    }

    /// Get all topics a subscriber is subscribed to by socket ID
    pub fn subscriber_topics_by_socket_id(&self, socket_id: &str) -> Vec<String> {
        if let Some(id) = self.get_subscriber_by_socket_id(socket_id) {
            self.subscriber_topics(id)
        } else {
            Vec::new()
        }
    }

    // =========================================================================
    // Core Subscribe/Unsubscribe Operations
    // =========================================================================

    /// Subscribe to a topic
    ///
    /// Messages published to this topic will be sent to the subscriber.
    ///
    /// # Arguments
    ///
    /// * `id` - The subscriber ID
    /// * `topic` - The topic name to subscribe to
    ///
    /// # Returns
    ///
    /// `true` if newly subscribed, `false` if already subscribed
    pub fn subscribe(&self, id: SubscriberId, topic: &str) -> bool {
        // Add topic to subscriber's set
        let subscriber = {
            let subs = self.subscribers.read();
            subs.get(&id).cloned()
        };

        if let Some(sub) = subscriber {
            let mut topics = sub.topics.write();
            if !topics.insert(topic.to_string()) {
                return false; // Already subscribed
            }
        } else {
            return false; // Subscriber doesn't exist
        }

        // Add subscriber to topic
        let shard = self.shard(topic);
        let mut topics = shard.topics.write();

        let topic_entry = topics.entry(topic.to_string()).or_insert_with(|| {
            self.topic_count.fetch_add(1, Ordering::Relaxed);
            Topic::default()
        });

        topic_entry.subscribers.insert(id)
    }

    /// Unsubscribe from a topic
    ///
    /// # Arguments
    ///
    /// * `id` - The subscriber ID
    /// * `topic` - The topic name to unsubscribe from
    ///
    /// # Returns
    ///
    /// `true` if was subscribed, `false` if wasn't subscribed
    pub fn unsubscribe(&self, id: SubscriberId, topic: &str) -> bool {
        // Remove topic from subscriber's set
        let subscriber = {
            let subs = self.subscribers.read();
            subs.get(&id).cloned()
        };

        if let Some(sub) = subscriber {
            sub.topics.write().remove(topic);
        }

        self.unsubscribe_internal(id, topic)
    }

    /// Internal unsubscribe (doesn't update subscriber's topic set)
    fn unsubscribe_internal(&self, id: SubscriberId, topic: &str) -> bool {
        let shard = self.shard(topic);
        let mut topics = shard.topics.write();

        if let Some(topic_entry) = topics.get_mut(topic) {
            let removed = topic_entry.subscribers.remove(&id);

            // Remove empty topics
            if topic_entry.subscribers.is_empty() {
                topics.remove(topic);
                self.topic_count.fetch_sub(1, Ordering::Relaxed);
            }

            removed
        } else {
            false
        }
    }

    // =========================================================================
    // Publish Operations
    // =========================================================================

    /// Publish a message to all subscribers of a topic
    ///
    /// The message is cloned for each subscriber (zero-copy due to `Bytes`).
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic to publish to
    /// * `message` - The message to publish
    ///
    /// # Returns
    ///
    /// Result indicating how many subscribers received the message
    pub fn publish(&self, topic: &str, message: Message) -> PublishResult {
        self.publish_impl(topic, message, None)
    }

    /// Publish a message to all subscribers except one
    ///
    /// This is commonly used when a connection wants to broadcast
    /// to others but not receive its own message.
    ///
    /// # Arguments
    ///
    /// * `exclude` - The subscriber ID to exclude
    /// * `topic` - The topic to publish to
    /// * `message` - The message to publish
    ///
    /// # Returns
    ///
    /// Result indicating how many subscribers received the message
    pub fn publish_excluding(
        &self,
        exclude: SubscriberId,
        topic: &str,
        message: Message,
    ) -> PublishResult {
        self.publish_impl(topic, message, Some(exclude))
    }

    /// Internal publish implementation
    fn publish_impl(
        &self,
        topic: &str,
        message: Message,
        exclude: Option<SubscriberId>,
    ) -> PublishResult {
        let shard = self.shard(topic);
        let topics = shard.topics.read();

        let topic_entry = match topics.get(topic) {
            Some(t) => t,
            None => return PublishResult::NoSubscribers,
        };

        if topic_entry.subscribers.is_empty() {
            return PublishResult::NoSubscribers;
        }

        // Get subscriber senders
        let subscribers = self.subscribers.read();
        let mut sent = 0;

        for &sub_id in &topic_entry.subscribers {
            if Some(sub_id) == exclude {
                continue;
            }

            if let Some(subscriber) = subscribers.get(&sub_id) {
                // Clone is O(1) for Message because it uses Bytes internally
                if subscriber.sender.send(message.clone()).is_ok() {
                    sent += 1;
                }
            }
        }

        self.messages_published.fetch_add(1, Ordering::Relaxed);

        if sent > 0 {
            PublishResult::Published(sent)
        } else {
            PublishResult::NoSubscribers
        }
    }

    // =========================================================================
    // Query Operations
    // =========================================================================

    /// Check if a subscriber is subscribed to a topic
    pub fn is_subscribed(&self, id: SubscriberId, topic: &str) -> bool {
        let shard = self.shard(topic);
        let topics = shard.topics.read();

        topics
            .get(topic)
            .map(|t| t.subscribers.contains(&id))
            .unwrap_or(false)
    }

    /// Get the number of subscribers to a topic
    pub fn topic_subscriber_count(&self, topic: &str) -> usize {
        let shard = self.shard(topic);
        let topics = shard.topics.read();

        topics.get(topic).map(|t| t.subscribers.len()).unwrap_or(0)
    }

    /// Get the total number of topics (with at least one subscriber)
    pub fn topic_count(&self) -> usize {
        self.topic_count.load(Ordering::Relaxed)
    }

    /// Get the total number of subscribers
    pub fn subscriber_count(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// Get the total number of messages published
    pub fn messages_published(&self) -> u64 {
        self.messages_published.load(Ordering::Relaxed)
    }

    /// Get all topics a subscriber is subscribed to
    pub fn subscriber_topics(&self, id: SubscriberId) -> Vec<String> {
        let subscribers = self.subscribers.read();

        subscribers
            .get(&id)
            .map(|sub| sub.topics.read().iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all topic names in the system
    pub fn all_topics(&self) -> Vec<String> {
        let mut all_topics = Vec::new();

        for shard in self.shards.iter() {
            let topics = shard.topics.read();
            for topic in topics.keys() {
                all_topics.push(topic.clone());
            }
        }

        all_topics
    }

    /// Get all socket IDs in the system
    pub fn all_socket_ids(&self) -> Vec<String> {
        self.socket_id_map.read().keys().cloned().collect()
    }

    /// Check if a socket ID exists
    pub fn has_socket_id(&self, socket_id: &str) -> bool {
        self.socket_id_map.read().contains_key(socket_id)
    }
}

impl Default for PubSub {
    fn default() -> Self {
        Self::new()
    }
}

// Safe to send and share across threads
unsafe impl Send for PubSub {}
unsafe impl Sync for PubSub {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn test_subscriber_lifecycle() {
        let pubsub = PubSub::new();

        // Create subscriber
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(tx);

        assert_eq!(pubsub.subscriber_count(), 1);

        // Remove subscriber
        pubsub.remove_subscriber(id);
        assert_eq!(pubsub.subscriber_count(), 0);
    }

    #[test]
    fn test_subscribe_unsubscribe() {
        let pubsub = PubSub::new();

        let (tx, _rx) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(tx);

        // Subscribe
        assert!(pubsub.subscribe(id, "topic1"));
        assert!(pubsub.is_subscribed(id, "topic1"));
        assert_eq!(pubsub.topic_count(), 1);
        assert_eq!(pubsub.topic_subscriber_count("topic1"), 1);

        // Double subscribe returns false
        assert!(!pubsub.subscribe(id, "topic1"));

        // Unsubscribe
        assert!(pubsub.unsubscribe(id, "topic1"));
        assert!(!pubsub.is_subscribed(id, "topic1"));
        assert_eq!(pubsub.topic_count(), 0);
    }

    #[tokio::test]
    async fn test_publish() {
        let pubsub = PubSub::new();

        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();

        let id1 = pubsub.create_subscriber(tx1);
        let id2 = pubsub.create_subscriber(tx2);

        pubsub.subscribe(id1, "chat");
        pubsub.subscribe(id2, "chat");

        // Publish to both
        let result = pubsub.publish("chat", Message::text("hello"));
        assert_eq!(result, PublishResult::Published(2));

        // Both receive
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_publish_excluding() {
        let pubsub = PubSub::new();

        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();

        let id1 = pubsub.create_subscriber(tx1);
        let id2 = pubsub.create_subscriber(tx2);

        pubsub.subscribe(id1, "chat");
        pubsub.subscribe(id2, "chat");

        // Publish excluding id1
        let result = pubsub.publish_excluding(id1, "chat", Message::text("hello"));
        assert_eq!(result, PublishResult::Published(1));

        // Only id2 receives
        assert!(rx1.try_recv().is_err());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn test_publish_no_subscribers() {
        let pubsub = PubSub::new();

        let result = pubsub.publish("nonexistent", Message::text("hello"));
        assert_eq!(result, PublishResult::NoSubscribers);
    }

    #[test]
    fn test_remove_subscriber_cleans_topics() {
        let pubsub = PubSub::new();

        let (tx, _rx) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(tx);

        pubsub.subscribe(id, "topic1");
        pubsub.subscribe(id, "topic2");
        assert_eq!(pubsub.topic_count(), 2);

        // Remove subscriber should clean up topics
        pubsub.remove_subscriber(id);
        assert_eq!(pubsub.topic_count(), 0);
    }

    #[test]
    fn test_sharding_distribution() {
        let pubsub = PubSub::new();

        // Create many topics to test distribution
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(tx);

        for i in 0..1000 {
            pubsub.subscribe(id, &format!("topic_{}", i));
        }

        assert_eq!(pubsub.topic_count(), 1000);
        assert_eq!(pubsub.all_topics().len(), 1000);
    }

    // =========================================================================
    // Pusher-style Socket ID Tests
    // =========================================================================

    #[test]
    fn test_pusher_style_socket_id() {
        let pubsub = PubSub::new();

        let (tx, _rx) = mpsc::unbounded_channel();
        let socket_id = "1234.5678";

        let id = pubsub.create_subscriber_with_id(socket_id, tx);

        // Verify socket ID mapping
        assert_eq!(pubsub.get_subscriber_by_socket_id(socket_id), Some(id));
        assert_eq!(pubsub.get_socket_id(id), Some(socket_id.to_string()));
        assert!(pubsub.has_socket_id(socket_id));
    }

    #[test]
    fn test_subscribe_by_socket_id() {
        let pubsub = PubSub::new();

        let (tx, _rx) = mpsc::unbounded_channel();
        let socket_id = "1234.5678";

        pubsub.create_subscriber_with_id(socket_id, tx);

        // Subscribe using socket ID
        assert!(pubsub.subscribe_by_socket_id(socket_id, "private-chat"));
        assert!(pubsub.is_subscribed_by_socket_id(socket_id, "private-chat"));

        // Unsubscribe using socket ID
        assert!(pubsub.unsubscribe_by_socket_id(socket_id, "private-chat"));
        assert!(!pubsub.is_subscribed_by_socket_id(socket_id, "private-chat"));
    }

    #[tokio::test]
    async fn test_publish_excluding_socket_id() {
        let pubsub = PubSub::new();

        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();

        let socket_id1 = "1111.2222";
        let socket_id2 = "3333.4444";

        pubsub.create_subscriber_with_id(socket_id1, tx1);
        pubsub.create_subscriber_with_id(socket_id2, tx2);

        pubsub.subscribe_by_socket_id(socket_id1, "chat");
        pubsub.subscribe_by_socket_id(socket_id2, "chat");

        // Publish excluding socket_id1
        let result = pubsub.publish_excluding_socket_id(socket_id1, "chat", Message::text("hello"));
        assert_eq!(result, PublishResult::Published(1));

        // Only socket_id2 receives
        assert!(rx1.try_recv().is_err());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn test_remove_subscriber_by_socket_id() {
        let pubsub = PubSub::new();

        let (tx, _rx) = mpsc::unbounded_channel();
        let socket_id = "1234.5678";

        pubsub.create_subscriber_with_id(socket_id, tx);
        pubsub.subscribe_by_socket_id(socket_id, "topic1");

        assert_eq!(pubsub.subscriber_count(), 1);
        assert_eq!(pubsub.topic_count(), 1);

        // Remove by socket ID
        assert!(pubsub.remove_subscriber_by_socket_id(socket_id));
        assert_eq!(pubsub.subscriber_count(), 0);
        assert_eq!(pubsub.topic_count(), 0);
        assert!(!pubsub.has_socket_id(socket_id));
    }

    #[test]
    fn test_generate_socket_id() {
        let id1 = PubSub::generate_socket_id();
        let id2 = PubSub::generate_socket_id();

        // Should contain a dot
        assert!(id1.contains('.'));
        assert!(id2.contains('.'));

        // Note: IDs might be the same if called in quick succession,
        // but they should have the format "number.number"
        let parts: Vec<&str> = id1.split('.').collect();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_create_subscriber_with_id_or_get() {
        let pubsub = PubSub::new();

        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let socket_id = "1234.5678";

        // First call creates
        let (id1, created1) = pubsub.create_subscriber_with_id_or_get(socket_id, tx1);
        assert!(created1);

        // Second call returns existing
        let (id2, created2) = pubsub.create_subscriber_with_id_or_get(socket_id, tx2);
        assert!(!created2);
        assert_eq!(id1, id2);

        // Only one subscriber
        assert_eq!(pubsub.subscriber_count(), 1);
    }

    #[test]
    fn test_all_socket_ids() {
        let pubsub = PubSub::new();

        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();

        pubsub.create_subscriber_with_id("1111.2222", tx1);
        pubsub.create_subscriber_with_id("3333.4444", tx2);

        let socket_ids = pubsub.all_socket_ids();
        assert_eq!(socket_ids.len(), 2);
        assert!(socket_ids.contains(&"1111.2222".to_string()));
        assert!(socket_ids.contains(&"3333.4444".to_string()));
    }

    #[test]
    fn test_subscriber_topics_by_socket_id() {
        let pubsub = PubSub::new();

        let (tx, _rx) = mpsc::unbounded_channel();
        let socket_id = "1234.5678";

        pubsub.create_subscriber_with_id(socket_id, tx);
        pubsub.subscribe_by_socket_id(socket_id, "topic1");
        pubsub.subscribe_by_socket_id(socket_id, "topic2");

        let topics = pubsub.subscriber_topics_by_socket_id(socket_id);
        assert_eq!(topics.len(), 2);
        assert!(topics.contains(&"topic1".to_string()));
        assert!(topics.contains(&"topic2".to_string()));
    }
}
