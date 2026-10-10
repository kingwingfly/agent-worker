use agent_runner::Config;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    agent_runner::harden_process();

    let runner = agent_runner::start(Config::from_env()?).await?;
    tracing::info!(addr = %runner.addr, approver = %runner.approver_addr, "runner listening");

    // Cloudflare stops a container with SIGTERM. State is saved at the end of every turn,
    // so there is nothing to flush: returning drops the runtime, which kills `claude`.
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = runner.wait() => result,
        _ = terminate.recv() => Ok(()),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
