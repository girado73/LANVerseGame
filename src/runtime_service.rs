//! Builds the production service and control plane from settings.

use std::sync::Arc;

use crate::api::ControlPlane;
use crate::auth::TokenService;
use crate::db::Db;
use crate::net::router::InProcessRouter;
use crate::net::service::DefaultMatchmaker;
use crate::net::service::{GameService, ResultSink, WorldStateStore};
use crate::settings::Settings;
use crate::store::modes::ModeLibrary;
use crate::store::redis::RedisStore;
use crate::store::results::ResultRecorder;

/// The durable side of the process (database, cache, tokens, modes).
pub struct Control {
    pub db: Option<Db>,
    pub redis: Option<Arc<RedisStore>>,
}

impl Control {
    /// Connects whatever the settings provide; each piece is optional so a
    /// protocol-only server still starts.
    pub async fn connect(settings: &Settings) -> Self {
        let db = match &settings.database_url {
            Some(url) => match Db::connect(url, 10).await {
                Ok(db) => {
                    if let Err(error) = db.migrate().await {
                        tracing::error!(%error, "database migration failed");
                    }
                    Some(db)
                }
                Err(error) => {
                    tracing::error!(%error, "database unavailable");
                    None
                }
            },
            None => None,
        };

        let redis = match RedisStore::connect(&settings.redis_url) {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                tracing::warn!(%error, "redis unavailable");
                None
            }
        };

        Self { db, redis }
    }

    /// A copy for the loop thread (connections are pool-backed and shareable).
    pub fn shared(&self) -> Control {
        Control {
            db: self.db.clone(),
            redis: self.redis.clone(),
        }
    }

    /// The control plane the HTTP handlers use.
    pub fn control_plane(&self, settings: &Settings) -> ControlPlane {
        let tokens = self
            .db
            .clone()
            .map(|db| Arc::new(TokenService::new(db.clone())));
        let modes = self.db.clone().map(|db| {
            Arc::new(ModeLibrary::new(
                db,
                self.redis.clone(),
                settings.modes.allow_publish,
                settings.modes.max_script_bytes,
            ))
        });

        let results = settings
            .result_secret
            .as_ref()
            .and_then(|secret| self.db.clone().map(|db| (db, secret.clone())))
            .map(|(db, secret)| {
                Arc::new(ResultRecorder::new(db, secret).with_match_defaults(
                    &settings.realtime.game_mode,
                    settings.simulation.tick_rate,
                    settings.simulation.max_players,
                ))
            });

        ControlPlane {
            db: self.db.clone(),
            redis: self.redis.clone(),
            tokens,
            modes,
            results,
            secret: settings.result_secret.clone(),
            admin_key: settings.admin_key.clone(),
        }
    }

    /// Everything the game loop hands results to.
    fn result_sink(&self, settings: &Settings) -> Arc<dyn ResultSink> {
        match (&self.db, &settings.result_secret) {
            (Some(db), Some(secret)) => Arc::new(
                ResultRecorder::new(db.clone(), secret.clone()).with_match_defaults(
                    &settings.realtime.game_mode,
                    settings.simulation.tick_rate,
                    settings.simulation.max_players,
                ),
            ),
            _ => Arc::new(NullResults),
        }
    }

    fn state_store(&self) -> Arc<dyn WorldStateStore> {
        match &self.redis {
            Some(redis) => Arc::new(RedisStateStore(redis.clone())),
            None => Arc::new(NullStore),
        }
    }
}

/// Assembles the [`GameService`] the loop thread owns.
pub fn build_service(
    settings: &Settings,
    router: Arc<InProcessRouter>,
    control: Control,
) -> GameService {
    let service = GameService::new(
        settings.simulation.clone(),
        settings.realtime.protocol_version,
        settings.realtime.idle_timeout_ms,
        settings.realtime.flush_interval_ms,
        &settings.realtime.game_mode,
    )
    .with_router(router)
    .with_matchmaker(Arc::new(DefaultMatchmaker::new(
        &settings.realtime.default_match,
    )))
    .with_results(control.result_sink(settings))
    .with_state_store(control.state_store());

    if let Some(db) = &control.db {
        let tokens = Arc::new(TokenService::new(db.clone()));
        let service = service.with_identity(Arc::new(crate::auth::Resolver::new(tokens)));

        let modes = Arc::new(ModeLibrary::new(
            db.clone(),
            control.redis.clone(),
            settings.modes.allow_publish,
            settings.modes.max_script_bytes,
        ));
        service.with_modes(modes.as_repository())
    } else {
        service
    }
}

/// Writes worlds to Redis (`lanbash:match:<id>:state`).
pub struct RedisStateStore(pub Arc<RedisStore>);

impl WorldStateStore for RedisStateStore {
    fn save_states(&self, states: &[(String, serde_json::Value)]) {
        let redis = self.0.clone();
        let states = states.to_vec();
        crate::blocking::run(redis.save_world_states("lanverse", &states));
    }
}

pub struct NullStore;

impl WorldStateStore for NullStore {
    fn save_states(&self, _states: &[(String, serde_json::Value)]) {}
}

pub struct NullResults;

impl ResultSink for NullResults {
    fn record(&self, _result: &crate::net::service::RoundResult) {}
    fn match_started(&self, _match_id: &str) {}
    fn participant(
        &self,
        _match_id: &str,
        _player_id: &str,
        _identity: &crate::net::service::GameIdentity,
        _team: Option<&str>,
    ) {
    }
}
