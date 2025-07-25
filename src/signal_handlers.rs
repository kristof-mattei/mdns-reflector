use tokio::signal::unix::{SignalKind, signal};

/// Waits forever for a SIGTERM
pub async fn wait_for_sigterm() -> Option<()> {
    signal(SignalKind::terminate())
        .expect("Failed to register SIGTERM handler")
        .recv()
        .await
}

/// Waits forever for a SIGUSR1
#[expect(unused, reason = "Library code")]
pub async fn wait_for_sigusr1() -> Option<()> {
    signal(SignalKind::user_defined1())
        .expect("Failed to register SIGUSR1 handler")
        .recv()
        .await
}

/// Waits forever for a SIGINT
pub async fn wait_for_sigint() -> Option<()> {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to register SIGINT (CTRL+C) handler");

    Some(())
}
