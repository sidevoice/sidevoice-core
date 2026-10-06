use super::*;
use crate::server::rendezvous::test_support::loopback;

/// A channel whose WebSocket side never drains, so the next send must time out.
/// The caller keeps the returned receiver alive: dropped, the send would fail
/// at once instead of timing out.
async fn block_channel(relay: &Relay) -> mpsc::Receiver<Message> {
    let (sender, receiver) = mpsc::channel(1);
    sender.send(Message::text("held")).await.unwrap();
    let task = tokio::spawn(std::future::pending());
    relay
        .channels
        .lock()
        .await
        .insert("blocked".into(), Channel { sender, task });
    receiver
}

fn data(payload: Part) -> Part {
    Part::object([("channel", Part::Text("blocked".into())), ("data", payload)])
}

#[tokio::test]
async fn full_channel_reports_one_close_or_stops_the_link() {
    let (outbound, mut room) = mpsc::channel(1);
    let relay = Arc::new(Relay::new(loopback(), outbound));
    let _receiver = block_channel(&relay).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        relay.handle("relay.data", data(Part::Binary(vec![0, 1]))),
    )
    .await
    .unwrap();
    let (event, frame) = room.recv().await.unwrap();
    assert_eq!(event, "relay.close");
    assert_eq!(frame.get("channel").and_then(Part::text), Some("blocked"));
    assert!(relay.channels.lock().await.is_empty());
    let _ = relay
        .handle("relay.data", data(Part::Text("late".into())))
        .await;
    assert!(
        room.try_recv().is_err(),
        "the room must see exactly one close"
    );
    tokio::time::timeout(Duration::from_secs(1), relay.shutdown())
        .await
        .unwrap();

    let (outbound, _room) = mpsc::channel(1);
    outbound.send(("relay.data", Part::Null)).await.unwrap();
    let relay = Arc::new(Relay::new(loopback(), outbound));
    let _receiver = block_channel(&relay).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        relay.handle("relay.data", data(Part::Binary(vec![0, 1]))),
    )
    .await
    .unwrap();
    let stopped = relay.stopped();
    assert!(
        *stopped.borrow(),
        "an undeliverable close must stop the link"
    );
    relay.shutdown().await;
}

#[tokio::test]
async fn in_flight_budget_refuses_excess_and_shutdown_cancels_request() {
    let (outbound, _) = mpsc::channel(1);
    let relay = Arc::new(Relay::new(loopback(), outbound));
    let permits = relay.requests.acquire_many(32).await.unwrap();
    let answer = relay
        .handle(
            "relay.http",
            Part::object([("path", Part::Text("/api/presentation".into()))]),
        )
        .await
        .unwrap();
    assert_eq!(answer.get("status"), Some(&Part::json(json!(503))));
    drop(permits);
    relay.shutdown().await;
    assert!(relay.handle("relay.http", Part::Null).await.is_none());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let (outbound, _) = mpsc::channel(1);
    let relay = Arc::new(Relay::new(base, outbound));
    let request_relay = relay.clone();
    let request = tokio::spawn(async move {
        request_relay
            .handle(
                "relay.http",
                Part::object([("path", Part::Text("/api/presentation".into()))]),
            )
            .await
    });
    let (_socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    relay.shutdown().await;
    assert!(tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .unwrap()
        .unwrap()
        .is_none());
}
