//! Inter-agent message bus.
//!
//! Topology semantics (cf. §4 / §5 of the spec):
//!   * `none` — agents run isolated; no envelopes are delivered.
//!   * `broadcast` — every published envelope is delivered to every other agent.
//!   * `directed` — envelopes are delivered only to agents whose `subscribes`
//!                  list includes the envelope topic.
//!
//! Visibility semantics:
//!   * `live` — envelopes are forwarded the moment they are published.
//!   * `post_output` — envelopes are buffered and only flushed once the
//!                    publishing agent has emitted its terminal `OUTPUT_READY`
//!                    sentinel (the agent runner emits this on stdout when its
//!                    output file is fully written).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{unbounded, Receiver, Sender};
use serde::{Deserialize, Serialize};

use crate::manifest::{MessageBus, Topology, Visibility};

/// One message on the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub from: String,
    /// `None` = broadcast. `Some(id)` = direct message to one agent.
    pub to: Option<String>,
    pub topic: String,
    pub body: String,
    /// Marks the publishing agent's "I'm done writing my output" signal so the
    /// `post_output` visibility mode can release buffered messages.
    #[serde(default)]
    pub output_ready: bool,
}

/// Public bus handle. Cheap to clone (internally Arc'd).
pub struct Bus {
    config: MessageBus,
    /// Inbound channel from agents → router.
    publisher: (Sender<Envelope>, Receiver<Envelope>),
    /// Per-agent receiver senders held by the router.
    subscribers: Arc<Mutex<HashMap<String, Sender<Envelope>>>>,
}

impl Bus {
    pub fn new(config: &MessageBus) -> Self {
        Self {
            config: config.clone(),
            publisher: unbounded(),
            subscribers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register an agent and return the receiver that delivers envelopes to it.
    pub fn subscribe(&self, agent_id: &str) -> Receiver<Envelope> {
        let (tx, rx) = unbounded();
        self.subscribers
            .lock()
            .unwrap()
            .insert(agent_id.to_string(), tx);
        rx
    }

    /// Sender used by every agent process to publish onto the bus.
    pub fn sender(&self) -> Sender<Envelope> {
        self.publisher.0.clone()
    }

    /// Spawn the routing thread. Returns a join handle; the thread exits when
    /// every clone of the publish sender drops.
    pub fn spawn_router(self) -> JoinHandle<()> {
        // Destructure to make the sender drop explicit. Otherwise the channel
        // would never close, since `Bus` (and its sender) would live for the
        // full thread lifetime via the closure capture below.
        let Bus {
            config,
            publisher: (publisher_sender, inbound),
            subscribers,
        } = self;
        drop(publisher_sender);
        thread::spawn(move || route(config, inbound, subscribers))
    }
}

fn route(
    cfg: MessageBus,
    inbound: Receiver<Envelope>,
    subscribers: Arc<Mutex<HashMap<String, Sender<Envelope>>>>,
) {
    // Buffered (per-author) envelopes for `post_output` visibility.
    let mut buffered: HashMap<String, Vec<Envelope>> = HashMap::new();
    // Subscriber routing-table (set of subscribed topics per agent), only used
    // for `directed` topology. Filled lazily from the first envelope each
    // agent emits with topic = "__subscribe__".
    let mut directed_topics: HashMap<String, Vec<String>> = HashMap::new();

    while let Ok(env) = inbound.recv() {
        if env.topic == "__subscribe__" {
            // Agent is announcing its subscription list.
            directed_topics.insert(env.from.clone(), split_topics(&env.body));
            continue;
        }

        let release_now = matches!(cfg.visibility, Visibility::Live) || env.output_ready;

        if !release_now {
            buffered.entry(env.from.clone()).or_default().push(env);
            continue;
        }

        // If output_ready, flush this author's buffered messages first.
        if env.output_ready {
            if let Some(prior) = buffered.remove(&env.from) {
                for queued in prior {
                    deliver(&cfg, &subscribers, &directed_topics, queued);
                }
            }
        }

        deliver(&cfg, &subscribers, &directed_topics, env);
    }

    // Drain any remaining buffered envelopes.
    for (_, list) in buffered.drain() {
        for env in list {
            deliver(&cfg, &subscribers, &directed_topics, env);
        }
    }
}

fn deliver(
    cfg: &MessageBus,
    subscribers: &Arc<Mutex<HashMap<String, Sender<Envelope>>>>,
    directed_topics: &HashMap<String, Vec<String>>,
    env: Envelope,
) {
    let subs = subscribers.lock().unwrap();
    match cfg.topology {
        Topology::None => { /* drop */ }
        Topology::Broadcast => {
            if let Some(target) = &env.to {
                if let Some(s) = subs.get(target) {
                    let _ = s.send(env.clone());
                }
            } else {
                for (id, s) in subs.iter() {
                    if id == &env.from {
                        continue;
                    }
                    let _ = s.send(env.clone());
                }
            }
        }
        Topology::Directed => {
            if let Some(target) = &env.to {
                if let Some(s) = subs.get(target) {
                    let _ = s.send(env.clone());
                }
            } else {
                for (id, s) in subs.iter() {
                    if id == &env.from {
                        continue;
                    }
                    let interested = directed_topics
                        .get(id)
                        .map(|topics| topics.iter().any(|t| t == &env.topic))
                        .unwrap_or(false);
                    if interested {
                        let _ = s.send(env.clone());
                    }
                }
            }
        }
    }
}

fn split_topics(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}
