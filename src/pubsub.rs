//! High-performance pub/sub system for WebSocket connections
//!
//! This module provides an ultra-fast topic-based publish/subscribe system
//! inspired by uWebSockets, designed for maximum throughput with:
//!
//! - **Linearized membership state**: Subscriber, socket, and topic indexes stay consistent
//! - **Lock-free subscriber IDs**: Atomic allocation of subscriber identifiers
//! - **Zero-copy messages**: Uses `Bytes` for efficient message sharing
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
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use tokio::sync::mpsc::UnboundedSender;

use crate::protocol::Message;

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
    topics: HashSet<String>,
    /// Optional Pusher-style socket ID (e.g., "1234.5678")
    socket_id: Option<String>,
}

#[derive(Default)]
struct PubSubData {
    /// Topics mapped to their subscriber sets
    topics: HashMap<String, HashSet<SubscriberId>>,
    /// All subscribers indexed by ID
    subscribers: HashMap<SubscriberId, Subscriber>,
    /// Socket ID to SubscriberId mapping (for Pusher-style IDs)
    socket_id_map: HashMap<String, SubscriberId>,
}

/// High-performance pub/sub state
///
/// This is the main entry point for the pub/sub system. It manages
/// topics, subscribers, and message delivery while keeping all indexes consistent.
///
/// # Thread Safety
///
/// `PubSub` is fully thread-safe and can be shared across async tasks. Membership
/// changes are linearized through one state lock. Publishing snapshots recipient
/// senders under a read lock and performs channel sends after releasing the lock.
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
    /// Authoritative subscriber, socket ID, and topic membership state
    state: RwLock<PubSubData>,
    /// Next subscriber ID (atomic counter)
    next_subscriber_id: AtomicU64,
    /// Total messages published (for stats)
    messages_published: AtomicU64,
}

/// Type alias for backward compatibility
pub type PubSubState = PubSub;

impl PubSub {
    /// Create a new pub/sub system
    pub fn new() -> Self {
        Self {
            state: RwLock::new(PubSubData::default()),
            next_subscriber_id: AtomicU64::new(1),
            messages_published: AtomicU64::new(0),
        }
    }

    fn insert_subscriber(
        data: &mut PubSubData,
        id: SubscriberId,
        sender: UnboundedSender<Message>,
        socket_id: Option<&str>,
    ) {
        let socket_id = socket_id.map(str::to_owned);
        let subscriber = Subscriber {
            sender,
            topics: HashSet::new(),
            socket_id: socket_id.clone(),
        };

        let previous = data.subscribers.insert(id, subscriber);
        debug_assert!(previous.is_none(), "subscriber IDs must be unique");

        if let Some(socket_id) = socket_id {
            // Insert into socket_id_map
            let previous = data.socket_id_map.insert(socket_id, id);
            debug_assert!(previous.is_none(), "socket IDs must be unique");
        }
    }

    fn remove_subscriber_from(data: &mut PubSubData, id: SubscriberId) -> bool {
        // Get the subscriber
        let Some(subscriber) = data.subscribers.remove(&id) else {
            return false;
        };

        // Remove from socket_id_map if present
        if let Some(socket_id) = subscriber.socket_id {
            let removed = data.socket_id_map.remove(&socket_id);
            debug_assert_eq!(removed, Some(id), "socket ID index must match subscriber");
        }

        // Unsubscribe from all topics
        for topic in subscriber.topics {
            let remove_topic = {
                let topic_subscribers = data
                    .topics
                    .get_mut(&topic)
                    .expect("subscriber topic must exist in topic index");
                let removed = topic_subscribers.remove(&id);
                debug_assert!(removed, "topic index must contain subscriber");
                topic_subscribers.is_empty()
            };

            if remove_topic {
                // Remove empty topics
                data.topics.remove(&topic);
            }
        }

        true
    }

    fn subscribe_in(data: &mut PubSubData, id: SubscriberId, topic: &str) -> bool {
        // Add topic to subscriber's set
        let Some(subscriber) = data.subscribers.get_mut(&id) else {
            // Subscriber doesn't exist
            return false;
        };

        if !subscriber.topics.insert(topic.to_owned()) {
            // Already subscribed
            return false;
        }

        // Add subscriber to topic
        let inserted = data.topics.entry(topic.to_owned()).or_default().insert(id);
        debug_assert!(
            inserted,
            "subscriber topic index must be updated atomically"
        );
        true
    }

    fn unsubscribe_in(data: &mut PubSubData, id: SubscriberId, topic: &str) -> bool {
        // Remove topic from subscriber's set
        let Some(subscriber) = data.subscribers.get_mut(&id) else {
            return false;
        };

        if !subscriber.topics.remove(topic) {
            return false;
        }

        let remove_topic = {
            let topic_subscribers = data
                .topics
                .get_mut(topic)
                .expect("subscriber topic must exist in topic index");
            let removed = topic_subscribers.remove(&id);
            debug_assert!(removed, "topic index must contain subscriber");
            topic_subscribers.is_empty()
        };

        if remove_topic {
            // Remove empty topics
            data.topics.remove(topic);
        }

        true
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
        Self::insert_subscriber(&mut self.state.write(), id, sender, None);

        id
    }

    /// Remove a subscriber and unsubscribe from all topics
    ///
    /// This should be called when a WebSocket connection closes.
    pub fn remove_subscriber(&self, id: SubscriberId) {
        Self::remove_subscriber_from(&mut self.state.write(), id);
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
        let mut data = self.state.write();
        // Check if already exists
        if data.socket_id_map.contains_key(socket_id) {
            panic!("Socket ID '{}' is already in use", socket_id);
        }

        // Create new subscriber
        let id = SubscriberId(self.next_subscriber_id.fetch_add(1, Ordering::Relaxed));
        Self::insert_subscriber(&mut data, id, sender, Some(socket_id));

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
        let mut data = self.state.write();
        // Check if already exists
        if let Some(id) = data.socket_id_map.get(socket_id) {
            return (*id, false);
        }

        // Create new subscriber
        let id = SubscriberId(self.next_subscriber_id.fetch_add(1, Ordering::Relaxed));
        Self::insert_subscriber(&mut data, id, sender, Some(socket_id));

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
        self.state.read().socket_id_map.get(socket_id).copied()
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
        self.state
            .read()
            .subscribers
            .get(&id)
            .and_then(|subscriber| subscriber.socket_id.clone())
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
        let mut data = self.state.write();
        let Some(id) = data.socket_id_map.get(socket_id).copied() else {
            return false;
        };

        Self::remove_subscriber_from(&mut data, id)
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
        let mut data = self.state.write();
        let Some(id) = data.socket_id_map.get(socket_id).copied() else {
            return false;
        };

        Self::subscribe_in(&mut data, id, topic)
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
        let mut data = self.state.write();
        let Some(id) = data.socket_id_map.get(socket_id).copied() else {
            return false;
        };

        Self::unsubscribe_in(&mut data, id, topic)
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
        let data = self.state.read();
        // Socket ID not found means publish to all subscribers.
        let exclude = data.socket_id_map.get(socket_id).copied();
        let recipients = Self::recipient_snapshot(&data, topic, exclude);
        drop(data);

        self.send_to_recipients(recipients, message)
    }

    /// Check if a socket ID is subscribed to a topic
    pub fn is_subscribed_by_socket_id(&self, socket_id: &str, topic: &str) -> bool {
        let data = self.state.read();
        let Some(id) = data.socket_id_map.get(socket_id) else {
            return false;
        };

        data.subscribers
            .get(id)
            .is_some_and(|subscriber| subscriber.topics.contains(topic))
    }

    /// Get all topics a subscriber is subscribed to by socket ID
    pub fn subscriber_topics_by_socket_id(&self, socket_id: &str) -> Vec<String> {
        let data = self.state.read();
        let Some(id) = data.socket_id_map.get(socket_id) else {
            return Vec::new();
        };

        data.subscribers
            .get(id)
            .map(|subscriber| subscriber.topics.iter().cloned().collect())
            .unwrap_or_default()
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
        Self::subscribe_in(&mut self.state.write(), id, topic)
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
        Self::unsubscribe_in(&mut self.state.write(), id, topic)
    }

    // =========================================================================
    // Publish Operations
    // =========================================================================

    /// Publish a message to all subscribers of a topic
    ///
    /// The message is cloned for each subscriber (zero-copy due to `Bytes`).
    /// Recipients are snapshotted atomically with membership changes, then sent
    /// outside the state lock. A concurrent removal ordered after that snapshot
    /// does not cancel delivery already selected by this publish operation.
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
        let recipients = Self::recipient_snapshot(&self.state.read(), topic, None);
        self.send_to_recipients(recipients, message)
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
        let recipients = Self::recipient_snapshot(&self.state.read(), topic, Some(exclude));
        self.send_to_recipients(recipients, message)
    }

    fn recipient_snapshot(
        data: &PubSubData,
        topic: &str,
        exclude: Option<SubscriberId>,
    ) -> Option<Vec<UnboundedSender<Message>>> {
        let topic_subscribers = data.topics.get(topic)?;
        let recipients = topic_subscribers
            .iter()
            .filter(|id| Some(**id) != exclude)
            .map(|id| {
                data.subscribers
                    .get(id)
                    .expect("topic index must reference an active subscriber")
                    .sender
                    .clone()
            })
            .collect();

        Some(recipients)
    }

    fn send_to_recipients(
        &self,
        recipients: Option<Vec<UnboundedSender<Message>>>,
        message: Message,
    ) -> PublishResult {
        let Some(recipients) = recipients else {
            return PublishResult::NoSubscribers;
        };

        let mut sent = 0;
        for sender in recipients {
            // Clone is O(1) for Message because it uses Bytes internally
            if sender.send(message.clone()).is_ok() {
                sent += 1;
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
        self.state
            .read()
            .subscribers
            .get(&id)
            .is_some_and(|subscriber| subscriber.topics.contains(topic))
    }

    /// Get the number of subscribers to a topic
    pub fn topic_subscriber_count(&self, topic: &str) -> usize {
        self.state
            .read()
            .topics
            .get(topic)
            .map(HashSet::len)
            .unwrap_or(0)
    }

    /// Get the total number of topics (with at least one subscriber)
    pub fn topic_count(&self) -> usize {
        self.state.read().topics.len()
    }

    /// Get the total number of subscribers
    pub fn subscriber_count(&self) -> usize {
        self.state.read().subscribers.len()
    }

    /// Get the total number of messages published
    pub fn messages_published(&self) -> u64 {
        self.messages_published.load(Ordering::Relaxed)
    }

    /// Get all topics a subscriber is subscribed to
    pub fn subscriber_topics(&self, id: SubscriberId) -> Vec<String> {
        self.state
            .read()
            .subscribers
            .get(&id)
            .map(|subscriber| subscriber.topics.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all topic names in the system
    pub fn all_topics(&self) -> Vec<String> {
        self.state.read().topics.keys().cloned().collect()
    }

    /// Get all socket IDs in the system
    pub fn all_socket_ids(&self) -> Vec<String> {
        self.state.read().socket_id_map.keys().cloned().collect()
    }

    /// Check if a socket ID exists
    pub fn has_socket_id(&self, socket_id: &str) -> bool {
        self.state.read().socket_id_map.contains_key(socket_id)
    }
}

impl Default for PubSub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use tokio::sync::mpsc;

    fn assert_consistent(pubsub: &PubSub) {
        let data = pubsub.state.read();

        for (id, subscriber) in &data.subscribers {
            if let Some(socket_id) = &subscriber.socket_id {
                assert_eq!(data.socket_id_map.get(socket_id), Some(id));
            }

            for topic in &subscriber.topics {
                assert!(
                    data.topics
                        .get(topic)
                        .is_some_and(|subscribers| subscribers.contains(id))
                );
            }
        }

        for (socket_id, id) in &data.socket_id_map {
            assert_eq!(
                data.subscribers
                    .get(id)
                    .and_then(|subscriber| subscriber.socket_id.as_ref()),
                Some(socket_id)
            );
        }

        for (topic, subscribers) in &data.topics {
            assert!(!subscribers.is_empty());
            for id in subscribers {
                assert!(
                    data.subscribers
                        .get(id)
                        .is_some_and(|subscriber| subscriber.topics.contains(topic))
                );
            }
        }
    }

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
    fn test_many_topics() {
        let pubsub = PubSub::new();

        // Create many topics to exercise the membership indexes
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
    fn concurrent_duplicate_socket_id_creates_exactly_one_subscriber() {
        let pubsub = Arc::new(PubSub::new());
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let pubsub = Arc::clone(&pubsub);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    barrier.wait();
                    pubsub.create_subscriber_with_id("1234.5678", tx)
                })
            })
            .collect();

        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(std::thread::JoinHandle::join)
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        assert_eq!(pubsub.subscriber_count(), 1);
        assert_consistent(&pubsub);
    }

    #[test]
    fn concurrent_idempotent_socket_id_creation_returns_one_subscriber() {
        let pubsub = Arc::new(PubSub::new());
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let pubsub = Arc::clone(&pubsub);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    barrier.wait();
                    pubsub.create_subscriber_with_id_or_get("1234.5678", tx)
                })
            })
            .collect();

        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("creation thread must not panic"))
            .collect();

        assert!(results[0].0 == results[1].0);
        assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
        assert_eq!(pubsub.subscriber_count(), 1);
        assert_consistent(&pubsub);
    }

    #[test]
    fn concurrent_membership_changes_and_removal_leave_no_stale_indexes() {
        let pubsub = Arc::new(PubSub::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(tx);
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let pubsub = Arc::clone(&pubsub);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..1_000 {
                    pubsub.subscribe(id, "topic");
                    std::thread::yield_now();
                    pubsub.unsubscribe(id, "topic");
                }
            }));
        }

        let remover = {
            let pubsub = Arc::clone(&pubsub);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                pubsub.remove_subscriber(id);
            })
        };

        barrier.wait();
        for handle in handles {
            handle.join().expect("membership thread must not panic");
        }
        remover.join().expect("removal thread must not panic");

        assert_eq!(pubsub.subscriber_count(), 0);
        assert_eq!(pubsub.topic_count(), 0);
        assert_consistent(&pubsub);
    }

    #[test]
    fn concurrent_publish_and_removal_follow_snapshot_order() {
        let pubsub = Arc::new(PubSub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(tx);
        pubsub.subscribe(id, "topic");
        let barrier = Arc::new(Barrier::new(3));

        let publisher = {
            let pubsub = Arc::clone(&pubsub);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                pubsub.publish("topic", Message::text("message"))
            })
        };
        let remover = {
            let pubsub = Arc::clone(&pubsub);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                pubsub.remove_subscriber(id);
            })
        };

        barrier.wait();
        let result = publisher.join().expect("publish thread must not panic");
        remover.join().expect("removal thread must not panic");

        match result {
            PublishResult::Published(1) => assert!(rx.try_recv().is_ok()),
            PublishResult::NoSubscribers => assert!(rx.try_recv().is_err()),
            PublishResult::Published(count) => panic!("unexpected recipient count: {count}"),
        }
        assert_consistent(&pubsub);
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
