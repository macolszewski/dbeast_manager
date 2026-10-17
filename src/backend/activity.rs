//! Activity Monitor polling: each monitor keeps one connection, takes a
//! sample (sessions and server counters) when the window asks for it and
//! reconnects after a failure. The UI schedules the next sample only after
//! the previous one arrived, so polls never pile up on a slow server.

use super::{Backend, Event, fmt_err};
use crate::activity::{self, Snapshot};
use crate::drivers::Connection;
use crate::model::{DataSource, Page};
use std::time::Duration;

/// A sample or a signal that takes longer reports an error and reconnects.
const TIMEOUT: Duration = Duration::from_secs(15);

impl Backend {
    /// The monitor's idle connection, taken out of the map so the lock is
    /// never held during I/O and one stuck server cannot stall other monitors.
    fn activity_take(&self, monitor: u64) -> Option<Connection> {
        self.activity.lock().unwrap().get_mut(&monitor).and_then(Option::take)
    }

    /// Returns a working connection, unless the window closed meanwhile.
    fn activity_put(connections: &super::ActivityConnections, monitor: u64, conn: Connection) {
        if let Some(slot @ None) = connections.lock().unwrap().get_mut(&monitor) {
            *slot = Some(conn);
        }
    }

    pub fn activity_sample(&self, monitor: u64, request: u64, ds: DataSource) {
        let send = self.send();
        self.activity.lock().unwrap().entry(monitor).or_default();
        let idle = self.activity_take(monitor);
        let connections = self.activity.clone();
        self.rt.spawn(async move {
            let result = tokio::time::timeout(TIMEOUT, async {
                let mut conn = match idle {
                    Some(conn) => conn,
                    None => Connection::open(&ds).await?,
                };
                let sessions = conn.execute(activity::SESSIONS_SQL, Page::ALL).await?;
                let server = conn.execute(activity::SERVER_SQL, Page::ALL).await?;
                // Only a working connection is kept; a failed one reconnects next time.
                Self::activity_put(&connections, monitor, conn);
                anyhow::Ok(Snapshot {
                    sessions: activity::parse_sessions(&sessions),
                    server: activity::parse_server(&server),
                    at: Some(std::time::Instant::now()),
                })
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("The server did not answer within {} s", TIMEOUT.as_secs())));
            send(Event::ActivitySample { monitor, request, result: result.map(Box::new).map_err(fmt_err) });
        });
    }

    /// `pg_cancel_backend` or `pg_terminate_backend`, on the monitor's
    /// connection when it is idle, otherwise on a new one.
    pub fn activity_signal(&self, monitor: u64, ds: DataSource, pid: i32, terminate: bool) {
        let send = self.send();
        let idle = self.activity_take(monitor);
        let connections = self.activity.clone();
        self.rt.spawn(async move {
            let function = if terminate { "pg_terminate_backend" } else { "pg_cancel_backend" };
            let result = tokio::time::timeout(TIMEOUT, async {
                let mut conn = match idle {
                    Some(conn) => conn,
                    None => Connection::open(&ds).await?,
                };
                let data = conn.execute(&format!("SELECT {function}({pid})"), Page::first(1)).await?;
                Self::activity_put(&connections, monitor, conn);
                anyhow::Ok(matches!(data.rows.first().and_then(|r| r.first()), Some(crate::model::Value::Bool(true))))
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("The server did not answer within {} s", TIMEOUT.as_secs())));
            send(Event::ActivitySignal { monitor, pid, terminate, result: result.map_err(fmt_err) });
        });
    }

    /// Closes a monitor's connection when its window closes; a sample still
    /// running drops its connection instead of returning it.
    pub fn activity_close(&self, monitor: u64) {
        if let Some(Some(conn)) = self.activity.lock().unwrap().remove(&monitor) {
            // Closing may wait for the network; not on the UI thread.
            self.rt.spawn(async move { drop(conn) });
        }
    }
}
