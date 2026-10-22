//! What ends a running process besides the operating system's request to leave (`platform::signal`) and the
//! in-band one (`POST /api/local/shutdown`): a quiet period without calls.

use std::sync::Arc;
use std::time::Duration;

use crate::server::AppState;

/// Resolve once no call has been open and no connector linked for `seconds`.
pub(super) async fn idle_for(state: Arc<AppState>, seconds: f64) {
    let quiet = Duration::from_secs_f64(seconds.max(0.01));
    let mut since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs_f64(seconds.clamp(0.01, 5.0))).await;
        // A linked connector keeps the core up as much as an open call does.
        if state.open_calls() > 0 || state.room.has_connector() {
            since = tokio::time::Instant::now();
        } else if since.elapsed() >= quiet {
            return;
        }
    }
}
