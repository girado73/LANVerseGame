//! State shared by the HTTP handlers and the socket tasks.

use std::sync::Arc;

use crate::net::loop_driver::StatsHandle;
use crate::net::router::InProcessRouter;
use crate::settings::Settings;

#[derive(Clone)]
pub struct AppState {
    /// Published game statistics (the loop owns the real service).
    pub stats: StatsHandle,
    pub router: Arc<InProcessRouter>,
    pub settings: Arc<Settings>,
    pub control: Arc<crate::api::ControlPlane>,
}
