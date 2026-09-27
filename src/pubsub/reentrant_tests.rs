use super::*;
use rstest::rstest;
use std::sync::Arc;
use std::task::{Context, Wake, Waker};
use tokio::sync::mpsc;

struct RemoveRecipients {
    pubsub: Arc<PubSub>,
    recipients: Vec<SubscriberId>,
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
