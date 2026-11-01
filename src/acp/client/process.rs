//! Delegation's process barrier, using the pinned SDK's stdio launch API.

use futures::future::{BoxFuture, FutureExt};
use tokio_util::sync::CancellationToken;

use super::{AcpAgent, ClientOptions, Connection, Error, HostHandlers, Result};

struct Process(async_process::Child, bool);

impl Process {
    fn terminate(&mut self) {
        // Never signal again after reaping, when the OS may reuse the process-group ID.
        if !std::mem::replace(&mut self.1, false) {
            return;
        }
        // spawn_process creates a Unix process group; terminate wrapper descendants too.
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.0.id()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.0.kill();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Future-drop/panic fallback; ordinary completion explicitly awaits status below.
        self.terminate();
    }
}

pub(super) async fn connect<T: Send, F>(
    agent: AcpAgent,
    handlers: HostHandlers,
    options: ClientOptions,
    cancellation: CancellationToken,
    run: F,
) -> Result<T>
where
    F: for<'a> FnOnce(&'a mut Connection) -> BoxFuture<'a, Result<T>> + Send,
{
    options.validate()?;
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let (stdin, stdout, mut stderr, child) =
        agent.spawn_process().map_err(|_| Error::Connection {
            stage: "delegate launch",
        })?;
    let mut child = Process(child, true);
    let grace = options.cleanup_timeout;
    let stopped = cancellation.clone();
    let connection = super::connect_transport(
        agent_client_protocol::ByteStreams::new(stdin, stdout),
        handlers,
        options,
        cancellation,
        run,
    );
    tokio::pin!(connection);
    // Drain and discard continuously; neither child stderr nor an SDK error becomes diagnostics.
    let mut sink = futures::io::sink();
    let drain = futures::io::copy(&mut stderr, &mut sink);
    tokio::pin!(drain);
    let result = std::panic::AssertUnwindSafe(async {
        tokio::select! {
            result = async {
                tokio::select! {
                    result = &mut connection => result,
                    _ = &mut drain => connection.await,
                }
            } => result,
            () = async {
                stopped.cancelled().await;
                // Bound even a peer that stops reading while the SDK drains its final writes.
                tokio::time::sleep(grace.saturating_mul(4)).await;
            } => Err(Error::Timeout { stage: "delegate transport cleanup" }),
        }
    })
    .catch_unwind()
    .await
    .unwrap_or(Err(Error::Connection {
        stage: "delegate supervision",
    }));
    child.terminate();
    tokio::time::timeout(grace, child.0.status())
        .await
        .map_err(|_| Error::Timeout {
            stage: "delegate process cleanup",
        })?
        .map_err(|_| Error::Connection {
            stage: "delegate process cleanup",
        })?;
    result
}
