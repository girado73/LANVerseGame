//! Runs the game loop on its own thread.
//!
//! The `GameService` owns Lua states and is deliberately not `Send` between
//! awaits (a script must never migrate between threads). It therefore lives on
//! a dedicated current-thread runtime: authoritative, unaffected by HTTP
//! request latency, and impossible to share accidentally.
//!
//! Socket tasks hand inbound events over through a plain mutex + condvar, so
//! nothing depends on which runtime wakes the loop. The loop polls it between
//! ticks (at most a millisecond apart) and drains everything that arrived.
//!
//! HTTP handlers read a small [`GameStats`] snapshot instead of the service.

use std::sync::{Arc, Mutex};

use crate::net::router::{InProcessRouter, Inbound};
use crate::net::service::GameService;

/// Read-only numbers the HTTP layer may look at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GameStats {
    pub active_worlds: usize,
    pub connected_players: usize,
    pub tick: i64,
}

/// Set by the HTTP side on shutdown; the loop persists and returns.
#[derive(Debug, Clone, Default)]
pub struct StopHandle(Arc<std::sync::atomic::AtomicBool>);

impl StopHandle {
    pub fn stop(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Shared statistics, written by the loop, read by handlers.
#[derive(Clone, Default)]
pub struct StatsHandle(Arc<Mutex<GameStats>>);

impl StatsHandle {
    pub fn get(&self) -> GameStats {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set(&self, stats: GameStats) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = stats;
    }
}

/// Builds the service on the loop's thread.
pub struct ServiceFactory<F> {
    build: F,
}

impl<F> ServiceFactory<F>
where
    F: FnOnce(Arc<InProcessRouter>) -> GameService,
{
    pub fn new(build: F) -> Self {
        Self { build }
    }
}

impl ServiceFactory<Box<dyn FnOnce(Arc<InProcessRouter>) -> GameService + Send>> {
    /// The production wiring: settings, database, cache, sinks and store.
    pub fn from_runtime(
        settings: crate::settings::Settings,
        control: crate::runtime_service::Control,
    ) -> Self {
        Self::new(Box::new(move |router: Arc<InProcessRouter>| {
            crate::runtime_service::build_service(&settings, router, control)
        }))
    }
}

/// Blocks the calling thread, building the service there and running the loop.
///
/// Returns when the process is shutting down; every world is persisted first.
pub fn run<F>(
    router: Arc<InProcessRouter>,
    stats: StatsHandle,
    stop: StopHandle,
    tick_rate: i64,
    factory: ServiceFactory<F>,
) where
    F: FnOnce(Arc<InProcessRouter>) -> GameService,
{
    // A plain thread, deliberately without a Tokio runtime: the service is
    // synchronous and the control plane's database handle owns the runtime it
    // needs (re-entering a runtime here would fail).
    {
        let (events, wakeup) = router.event_slot();
        // Cloned so the service can own the router while the loop borrows the
        // event slot.
        let mut service = (factory.build)(router.clone());
        let state_store = service.state_store();

        let period = std::time::Duration::from_secs_f64(1.0 / tick_rate.max(1) as f64);
        let mut next_tick = std::time::Instant::now();
        let mut last_published = GameStats::default();
        let mut last_publish = std::time::Instant::now();

        while !stop.stopped() {
            // Wait until the next tick or an inbound event, whichever is first.
            let wait = next_tick.saturating_duration_since(std::time::Instant::now());
            let mut queue = events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            if queue.is_empty() && !wait.is_zero() {
                // Wake up at least every 100 ms to notice a stop request.
                let wait = wait.min(std::time::Duration::from_millis(100));
                queue = wakeup
                    .wait_timeout(queue, wait)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }

            let batch: Vec<_> = std::mem::take(&mut *queue);
            drop(queue);

            for event in batch {
                match event.inbound {
                    Inbound::Open => service.handle_open(&event.connection),
                    Inbound::Frame(frame) => service.handle_frame(&event.connection, &frame),
                    Inbound::Close => service.handle_close(&event.connection),
                }
            }

            if std::time::Instant::now() >= next_tick {
                service.tick(chrono::Utc::now().timestamp_millis());
                next_tick += period;

                if next_tick < std::time::Instant::now() {
                    // Fell behind: resynchronize instead of replaying a backlog.
                    next_tick = std::time::Instant::now() + period;
                }
            }

            if last_publish.elapsed() >= std::time::Duration::from_millis(500) {
                let current = GameStats {
                    active_worlds: service.worlds().len(),
                    connected_players: service.connected_players(),
                    tick: chrono::Utc::now().timestamp(),
                };

                if current != last_published {
                    stats.set(current.clone());
                    last_published = current;
                }
                last_publish = std::time::Instant::now();
            }
        }

        // Graceful stop: persist every world once more.
        let states: Vec<(String, serde_json::Value)> = service
            .worlds()
            .iter()
            .map(|(match_id, world)| (match_id.clone(), world.snapshot_state()))
            .collect();

        if !states.is_empty() {
            tracing::info!(matches = states.len(), "persisting worlds before shutdown");
            state_store.save_states(&states);
        }
    }
}
