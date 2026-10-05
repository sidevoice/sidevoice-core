//! Outbound `/nodes` Socket.IO client. `sioc` owns framing, attachment
//! reassembly, namespace auth, ACK correlation and Engine.IO heartbeats.

use std::ops::ControlFlow;
use std::sync::Arc;

use sioc::client::{ClientBuilder, SocketReceiver, SocketSender};
use sioc::packet::Signal;
use sioc::prelude::TransportStrategy;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, MissedTickBehavior};

use super::link::Rendezvous;
use super::packet::Part;
use super::pairing::{public_origin, Pairing};
use super::relay::Relay;

mod wire;

const OUTBOUND_PATH: &str = "/api/connectors/link";
const OUTBOUND_NAMESPACE: &str = "/nodes";
const MAX_IN_FLIGHT: usize = 32;
const PAIRING_CHECK: Duration = Duration::from_secs(2);
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// One dial attempt. The parent watcher retries transport failures and stops
/// on a room-supplied refusal until the connector changes the pairing file.
pub(super) async fn run(rv: Arc<Rendezvous>, pairing: Pairing) -> Result<(), ()> {
    let url = url::Url::parse(&pairing.origin).map_err(|_| ())?;
    let client = ClientBuilder::new(url)
        .path(OUTBOUND_PATH.trim_start_matches('/'))
        .transport(TransportStrategy::WebSocket)
        .open()
        .map_err(|_| ())?;
    let auth = rv.identity(&pairing);
    let (sender, receiver) = client
        .connect_with(OUTBOUND_NAMESPACE, auth.to_string())
        .await
        .map_err(|_| ())?;
    let (outbound, output) = mpsc::channel::<(&'static str, Part)>(128);
    let relay = Arc::new(Relay::new(rv.base().clone(), outbound));
    let mut session = Session {
        rv,
        sender,
        relay,
        requests: JoinSet::new(),
        welcomed: false,
    };
    session.serve(&pairing, receiver, output).await;
    session.close().await;
    drop(client);
    Ok(())
}

/// One connected `/nodes` socket and the relay requests it is serving.
struct Session {
    rv: Arc<Rendezvous>,
    sender: SocketSender,
    relay: Arc<Relay>,
    requests: JoinSet<()>,
    welcomed: bool,
}

impl Session {
    /// Serve until the pairing changes, either side ends the link, or the
    /// Core stops.
    async fn serve(
        &mut self,
        pairing: &Pairing,
        mut receiver: SocketReceiver,
        mut output: mpsc::Receiver<(&'static str, Part)>,
    ) {
        let mut relay_stopped = self.relay.stopped();
        let mut pairing_check = tokio::time::interval(PAIRING_CHECK);
        pairing_check.set_missed_tick_behavior(MissedTickBehavior::Skip);
        pairing_check.tick().await;
        let mut stopping = self.rv.stopping();
        loop {
            tokio::select! {
                _ = pairing_check.tick() => {
                    if self.rv.current_pairing().as_ref() != Some(pairing) { break; }
                },
                finished = self.requests.join_next(), if !self.requests.is_empty() => { let _ = finished; },
                signal = receiver.recv() => match signal {
                    Some(Signal::Connect(_)) => {},
                    Some(Signal::ConnectError(error)) => {
                        self.rv.refused(error.message.to_string()).await;
                        break;
                    }
                    Some(Signal::Disconnect) | None => break,
                    Some(Signal::Event(event)) => {
                        let Some((name, data, id)) = wire::parse(event) else { continue; };
                        if self.on_event(name, data, id).await.is_break() { break; }
                    }
                },
                outbound = output.recv() => match outbound {
                    Some((event, part)) => {
                        if !matches!(
                            tokio::time::timeout(SEND_TIMEOUT, wire::emit(&self.sender, event, part)).await,
                            Ok(true)
                        ) {
                            break;
                        }
                    },
                    None => break,
                },
                _ = self.rv.changed() => break,
                _ = relay_stopped.changed() => break,
                _ = stopping.changed() => break,
            }
        }
    }

    async fn on_event(&mut self, name: String, data: Part, id: Option<u64>) -> ControlFlow<()> {
        match name.as_str() {
            "node.welcome" => {
                self.welcomed = true;
                let public = data
                    .get("public_url")
                    .and_then(Part::text)
                    .and_then(public_origin);
                self.rv.connected("outbound", public).await;
            }
            "node.revoked" => {
                let reason = data.get("reason").and_then(Part::text).map(str::to_owned);
                self.rv.revoked(reason).await;
                return ControlFlow::Break(());
            }
            "relay.http" | "relay.open" => self.request(name, data, id).await,
            "relay.data" | "relay.close" => {
                let _ = self.relay.handle(&name, data).await;
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// Answer a request in the background, or refuse it at once over budget.
    async fn request(&mut self, name: String, data: Part, id: Option<u64>) {
        if self.requests.len() >= MAX_IN_FLIGHT {
            if let Some(id) = id {
                let answer = Relay::busy(&name);
                let _ =
                    tokio::time::timeout(SEND_TIMEOUT, wire::acknowledge(&self.sender, id, answer))
                        .await;
            }
            return;
        }
        let relay = self.relay.clone();
        let sender = self.sender.clone();
        self.requests.spawn(async move {
            if let Some(answer) = relay.handle(&name, data).await {
                if let Some(id) = id {
                    wire::acknowledge(&sender, id, answer).await;
                }
            }
        });
    }

    async fn close(self) {
        let Self {
            rv,
            sender,
            relay,
            mut requests,
            welcomed,
        } = self;
        requests.abort_all();
        while requests.join_next().await.is_some() {}
        relay.shutdown().await;
        sender.disconnect().await;
        if welcomed {
            rv.disconnected("outbound").await;
        }
    }
}
