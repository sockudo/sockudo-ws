use super::*;
use rstest::rstest;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Wake, Waker};
use tokio::sync::mpsc;

struct RemoveRecipients {
    pubsub: Arc<PubSub>,
    recipients: Vec<SubscriberId>,
}

struct MessageOwner {
    dropped: Arc<AtomicBool>,
}

impl AsRef<[u8]> for MessageOwner {
    fn as_ref(&self) -> &[u8] {
        b"owned message"
    }
}

impl Drop for MessageOwner {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
    }
}

struct ConsumeDuringDelivery {
    receiver: parking_lot::Mutex<mpsc::UnboundedReceiver<Message>>,
    owner_dropped: Arc<AtomicBool>,
}

impl Wake for ConsumeDuringDelivery {
    fn wake(self: Arc<Self>) {
        drop(self.receiver.lock().try_recv().unwrap());
        assert!(!self.owner_dropped.load(Ordering::Relaxed));
    }
}

#[test]
fn original_message_owner_survives_synchronous_receiver_consumption() {
    let pubsub = PubSub::new();
    let (sender, receiver) = mpsc::unbounded_channel();
    let id = pubsub.create_subscriber(sender);
    pubsub.subscribe(id, "topic");
    let dropped = Arc::new(AtomicBool::new(false));
    let observer = Arc::new(ConsumeDuringDelivery {
        receiver: parking_lot::Mutex::new(receiver),
        owner_dropped: dropped.clone(),
    });
    let waker = Waker::from(observer.clone());
    assert!(
        observer
            .receiver
            .lock()
            .poll_recv(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let message = Message::Binary(bytes::Bytes::from_owner(MessageOwner {
        dropped: dropped.clone(),
    }));

    let result = pubsub.publish("topic", message);

    assert_eq!(result, PublishResult::Published(1));
    assert!(dropped.load(Ordering::Relaxed));
    assert_eq!(pubsub.messages_published(), 1);
}

#[rstest]
#[case::missing_topic(false, false, PublishResult::NoSubscribers, 0)]
#[case::one_recipient(true, false, PublishResult::Published(1), 1)]
#[case::excluded_only_recipient(true, true, PublishResult::NoSubscribers, 1)]
fn publish_counts_existing_topics_even_when_the_only_recipient_is_excluded(
    #[case] subscribed: bool,
    #[case] excluded: bool,
    #[case] expected: PublishResult,
    #[case] published: u64,
) {
    let pubsub = PubSub::new();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let id = pubsub.create_subscriber(sender);
    if subscribed {
        pubsub.subscribe(id, "topic");
    }

    let result = if excluded {
        pubsub.publish_excluding(id, "topic", Message::text("message"))
    } else {
        pubsub.publish("topic", Message::text("message"))
    };

    assert_eq!(result, expected);
    assert_eq!(pubsub.messages_published(), published);
    assert_eq!(
        receiver.try_recv().is_ok(),
        expected == PublishResult::Published(1)
    );
}

#[rstest]
#[case::numeric(|pubsub: &PubSub, id| pubsub.remove_subscriber(id))]
#[case::socket(|pubsub: &PubSub, _| assert!(pubsub.remove_subscriber_by_socket_id("subscriber")))]
fn removing_the_last_sender_wakes_its_receiver_after_unlocking(
    #[case] remove: fn(&PubSub, SubscriberId),
) {
    let pubsub = Arc::new(PubSub::new());
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let id = pubsub.create_subscriber_with_id("subscriber", sender);
    pubsub.subscribe(id, "topic");
    let waker = Waker::from(Arc::new(RemoveRecipients {
        pubsub: Arc::clone(&pubsub),
        recipients: vec![id],
    }));
    let mut context = Context::from_waker(&waker);
    assert!(receiver.poll_recv(&mut context).is_pending());

    remove(&pubsub, id);

    assert!(matches!(
        receiver.poll_recv(&mut context),
        std::task::Poll::Ready(None)
    ));
    assert_eq!(pubsub.subscriber_count(), 0);
    assert_eq!(pubsub.topic_count(), 0);
    assert!(!pubsub.has_socket_id("subscriber"));
}

impl Wake for RemoveRecipients {
    fn wake(self: Arc<Self>) {
        // Fail immediately instead of deadlocking the test if the operation still
        // holds the membership lock when the channel wakes its receiver.
        assert!(
            self.pubsub.state.try_write().is_some(),
            "membership lock must be released before waking receivers"
        );
        for &id in &self.recipients {
            self.pubsub.remove_subscriber(id);
        }
    }
}

#[rstest]
#[case::all(|pubsub: &PubSub, _| pubsub.publish("topic", Message::text("message")), 3)]
#[case::numeric_exclusion(|pubsub: &PubSub, excluded| pubsub.publish_excluding(excluded, "topic", Message::text("message")), 2)]
#[case::socket_exclusion(|pubsub: &PubSub, _| pubsub.publish_excluding_socket_id("excluded", "topic", Message::text("message")), 2)]
fn receiver_wakers_can_remove_members_without_cancelling_selected_deliveries(
    #[case] publish: fn(&PubSub, SubscriberId) -> PublishResult,
    #[case] expected_count: usize,
) {
    let pubsub = Arc::new(PubSub::new());
    let mut receivers = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(sender);
        pubsub.subscribe(id, "topic");
        ids.push(id);
        receivers.push(receiver);
    }
    let (sender, mut excluded_receiver) = mpsc::unbounded_channel();
    let excluded = pubsub.create_subscriber_with_id("excluded", sender);
    pubsub.subscribe(excluded, "topic");
    ids.push(excluded);
    let waker = Waker::from(Arc::new(RemoveRecipients {
        pubsub: Arc::clone(&pubsub),
        recipients: ids,
    }));
    let mut context = Context::from_waker(&waker);
    for receiver in &mut receivers {
        assert!(receiver.poll_recv(&mut context).is_pending());
    }

    let result = publish(&pubsub, excluded);

    assert_eq!(result, PublishResult::Published(expected_count));
    for receiver in &mut receivers {
        assert!(
            matches!(receiver.try_recv().unwrap(), Message::Text(text) if text.as_ref() == b"message")
        );
        assert!(receiver.try_recv().is_err());
    }
    if expected_count == 3 {
        assert!(
            matches!(excluded_receiver.try_recv().unwrap(), Message::Text(text) if text.as_ref() == b"message")
        );
    }
    assert!(excluded_receiver.try_recv().is_err());
    assert_eq!(pubsub.subscriber_count(), 0);
    assert_eq!(pubsub.topic_count(), 0);
    assert!(!pubsub.has_socket_id("excluded"));
}

struct SubscribeRecipient {
    pubsub: Arc<PubSub>,
    id: SubscriberId,
}

impl Wake for SubscribeRecipient {
    fn wake(self: Arc<Self>) {
        self.pubsub.subscribe(self.id, "topic");
    }
}

#[test]
fn reentrant_subscription_does_not_join_an_existing_publish_snapshot() {
    let pubsub = Arc::new(PubSub::new());
    let (sender, mut selected) = mpsc::unbounded_channel();
    let id = pubsub.create_subscriber(sender);
    pubsub.subscribe(id, "topic");
    let (sender, mut added) = mpsc::unbounded_channel();
    let added_id = pubsub.create_subscriber(sender);
    let waker = Waker::from(Arc::new(SubscribeRecipient {
        pubsub: pubsub.clone(),
        id: added_id,
    }));
    let mut cx = Context::from_waker(&waker);
    assert!(selected.poll_recv(&mut cx).is_pending());

    let result = pubsub.publish("topic", Message::text("selected"));

    assert_eq!(result, PublishResult::Published(1));
    assert_eq!(selected.try_recv().unwrap().as_bytes(), b"selected");
    assert!(matches!(
        added.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(pubsub.is_subscribed(added_id, "topic"));
}

struct RemoveExcludedRecipient {
    pubsub: Arc<PubSub>,
    excluded: SubscriberId,
    receiver: parking_lot::Mutex<mpsc::UnboundedReceiver<Message>>,
    called: AtomicBool,
}

impl Wake for RemoveExcludedRecipient {
    fn wake(self: Arc<Self>) {
        self.pubsub.remove_subscriber(self.excluded);
        assert!(matches!(
            self.receiver.lock().try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        self.called.store(true, Ordering::Relaxed);
    }
}

#[rstest]
#[case::numeric(|pubsub: &PubSub, id| pubsub.publish_excluding(id, "topic", Message::text("selected")))]
#[case::socket(|pubsub: &PubSub, _| pubsub.publish_excluding_socket_id("excluded", "topic", Message::text("selected")))]
fn excluded_sender_is_not_retained_during_reentrant_delivery(
    #[case] publish: fn(&PubSub, SubscriberId) -> PublishResult,
) {
    let pubsub = Arc::new(PubSub::new());
    let (sender, receiver) = mpsc::unbounded_channel();
    let excluded = pubsub.create_subscriber_with_id("excluded", sender);
    pubsub.subscribe(excluded, "topic");
    let (sender, mut selected) = mpsc::unbounded_channel();
    let id = pubsub.create_subscriber(sender);
    pubsub.subscribe(id, "topic");
    let observation = Arc::new(RemoveExcludedRecipient {
        pubsub: pubsub.clone(),
        excluded,
        receiver: parking_lot::Mutex::new(receiver),
        called: AtomicBool::new(false),
    });
    let waker = Waker::from(observation.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(selected.poll_recv(&mut cx).is_pending());

    let result = publish(&pubsub, excluded);

    assert_eq!(result, PublishResult::Published(1));
    assert!(observation.called.load(Ordering::Relaxed));
}

struct ObserveCompletedRecipient {
    pubsub: Arc<PubSub>,
    recipients: Arc<
        Vec<(
            SubscriberId,
            parking_lot::Mutex<mpsc::UnboundedReceiver<Message>>,
        )>,
    >,
    first: Arc<parking_lot::Mutex<Option<usize>>>,
    index: usize,
}

impl Wake for ObserveCompletedRecipient {
    fn wake(self: Arc<Self>) {
        self.recipients[self.index].1.lock().try_recv().unwrap();
        let previous = self.first.lock().replace(self.index);
        if let Some(previous) = previous {
            let (id, receiver) = &self.recipients[previous];
            self.pubsub.remove_subscriber(*id);
            assert!(matches!(
                receiver.lock().try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ));
        }
    }
}

#[test]
fn completed_recipient_is_not_retained_by_later_deliveries() {
    let pubsub = Arc::new(PubSub::new());
    let recipients = Arc::new(
        (0..2)
            .map(|_| {
                let (sender, receiver) = mpsc::unbounded_channel();
                let id = pubsub.create_subscriber(sender);
                pubsub.subscribe(id, "topic");
                (id, parking_lot::Mutex::new(receiver))
            })
            .collect::<Vec<_>>(),
    );
    let first = Arc::new(parking_lot::Mutex::new(None));
    for (index, (_, receiver)) in recipients.iter().enumerate() {
        let waker = Waker::from(Arc::new(ObserveCompletedRecipient {
            pubsub: pubsub.clone(),
            recipients: recipients.clone(),
            first: first.clone(),
            index,
        }));
        assert!(
            receiver
                .lock()
                .poll_recv(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }

    let result = pubsub.publish("topic", Message::text("selected"));

    assert_eq!(result, PublishResult::Published(2));
}

#[rstest]
#[case::unsubscribe(|pubsub: &PubSub, id| assert!(pubsub.unsubscribe(id, "topic")))]
#[case::remove(|pubsub: &PubSub, id| pubsub.remove_subscriber(id))]
fn removing_a_middle_member_preserves_later_member_cleanup(
    #[case] remove: fn(&PubSub, SubscriberId),
) {
    let pubsub = PubSub::new();
    let mut recipients = Vec::new();
    for _ in 0..3 {
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = pubsub.create_subscriber(sender);
        pubsub.subscribe(id, "topic");
        recipients.push((id, receiver));
    }

    remove(&pubsub, recipients[1].0);
    remove(&pubsub, recipients[2].0);
    let result = pubsub.publish("topic", Message::text("remaining"));

    assert_eq!(result, PublishResult::Published(1));
    assert_eq!(pubsub.topic_subscriber_count("topic"), 1);
    assert!(pubsub.is_subscribed(recipients[0].0, "topic"));
    assert!(!pubsub.is_subscribed(recipients[1].0, "topic"));
    assert!(!pubsub.is_subscribed(recipients[2].0, "topic"));
    assert_eq!(recipients[0].1.try_recv().unwrap().as_bytes(), b"remaining");
    assert!(recipients[1].1.try_recv().is_err());
    assert!(recipients[2].1.try_recv().is_err());
}

struct RemoveWithWeakObservation {
    pubsub: Arc<PubSub>,
    id: SubscriberId,
    sender: mpsc::WeakUnboundedSender<Message>,
}

impl Wake for RemoveWithWeakObservation {
    fn wake(self: Arc<Self>) {
        self.pubsub.remove_subscriber(self.id);
        assert!(self.sender.upgrade().is_some());
    }
}

#[test]
fn selected_delivery_keeps_weak_sender_alive_until_it_finishes() {
    let pubsub = Arc::new(PubSub::new());
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let weak = sender.downgrade();
    let id = pubsub.create_subscriber(sender);
    pubsub.subscribe(id, "topic");
    let waker = Waker::from(Arc::new(RemoveWithWeakObservation {
        pubsub: pubsub.clone(),
        id,
        sender: weak.clone(),
    }));
    assert!(
        receiver
            .poll_recv(&mut Context::from_waker(&waker))
            .is_pending()
    );

    let result = pubsub.publish("topic", Message::text("selected"));

    assert_eq!(result, PublishResult::Published(1));
    assert!(weak.upgrade().is_none());
    assert_eq!(receiver.try_recv().unwrap().as_bytes(), b"selected");
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}
