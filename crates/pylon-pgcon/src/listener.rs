//! A dedicated LISTEN/NOTIFY connection. Unlike `PgPool`'s pooled
//! connections — which are meant to be checked out briefly and returned —
//! a listener has to stay open for as long as the caller cares about
//! notifications, and receives out-of-band `NOTIFY` messages that arrive
//! independently of any query the caller issues. `tokio_postgres` models
//! this as a `Client` (for issuing `LISTEN`/`UNLISTEN`/ordinary queries)
//! paired with a `Connection` that must be polled continuously to drive
//! I/O *and* to surface `AsyncMessage::Notification`s — normally that
//! polling is done by spawning the connection future and ignoring
//! everything it yields, but here the poll loop is written by hand instead
//! so each `Notification` can be handed to `on_notification` as it arrives.

use crate::error::Result;
use crate::wire::ExtensionOids;
use crate::{execute_typed_on, query_typed_on};
use pylon_value::CachedValue;
use tokio_postgres::AsyncMessage;

pub use tokio_postgres::Notification;

/// A connection dedicated to LISTEN/NOTIFY, plus ordinary query execution
/// on the same connection — mirroring how `pylon.worker.IndexWorker` and
/// `pylon.cache.CacheInvalidationWorker` both listen *and* run queries
/// (claim/mark-done/mark-failed, cache eviction) on the one connection
/// they're constructed with today.
pub struct PgListener {
    client: tokio_postgres::Client,
}

impl PgListener {
    /// Opens a new, non-pooled connection and spawns a background task
    /// that drives it for the lifetime of the returned `PgListener`,
    /// calling `on_notification` once per `NOTIFY` received (on any
    /// channel — `listen`/`unlisten` control which channels the server
    /// actually sends). The task exits when the connection is closed or
    /// errors, which happens when the returned `PgListener` (and its
    /// `Client`) is dropped.
    pub async fn connect<F>(dsn: &str, on_notification: F) -> Result<Self>
    where
        F: Fn(Notification) + Send + 'static,
    {
        let pg_config: tokio_postgres::Config = dsn.parse()?;
        let (client, mut connection) = pg_config.connect(tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            loop {
                match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                    Some(Ok(AsyncMessage::Notification(n))) => on_notification(n),
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => break,
                }
            }
        });
        Ok(Self { client })
    }

    pub async fn listen(&self, channel: &str) -> Result<()> {
        self.client.batch_execute(&format!("LISTEN {}", quote_ident(channel))).await?;
        Ok(())
    }

    pub async fn unlisten(&self, channel: &str) -> Result<()> {
        self.client.batch_execute(&format!("UNLISTEN {}", quote_ident(channel))).await?;
        Ok(())
    }

    pub async fn query_typed(&self, sql: &str, params: &[CachedValue], ext: &ExtensionOids) -> Result<Vec<CachedValue>> {
        query_typed_on(&self.client, sql, params, ext).await
    }

    pub async fn execute_typed(&self, sql: &str, params: &[CachedValue]) -> Result<u64> {
        execute_typed_on(&self.client, sql, params).await
    }
}

/// Quotes a channel/savepoint name as a Postgres identifier (`"..."`,
/// doubling any embedded `"`). Every call site in this crate passes a fixed
/// literal name (`pylon_index_queue`, `pylon_cache_invalidate`, `pylon_dev`),
/// never user input, but the SQL these go into takes an identifier, not a
/// string literal, so it still needs identifier quoting to be well-formed.
pub(crate) fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN")
            .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5418/app".to_string())
    }

    #[tokio::test]
    #[ignore]
    async fn receives_a_notification_on_a_listened_channel() {
        use std::sync::{Arc, Mutex};
        let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        let listener = PgListener::connect(&test_dsn(), move |n| {
            received_clone.lock().unwrap().push((n.channel().to_string(), n.payload().to_string()));
        })
        .await
        .unwrap();
        listener.listen("pgcon_listener_test").await.unwrap();

        // A second, ordinary pooled connection sends the NOTIFY — matching
        // how a real trigger (a different backend/session) raises it.
        let notifier = crate::PgPool::connect(&test_dsn(), 1).await.unwrap();
        notifier.query_raw("NOTIFY pgcon_listener_test, 'hello'").await.unwrap();

        // The notification arrives on the listener's own background task,
        // asynchronously — poll briefly rather than assuming it's already
        // there the instant NOTIFY returns.
        for _ in 0..50 {
            if !received.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let got = received.lock().unwrap().clone();
        assert_eq!(got, vec![("pgcon_listener_test".to_string(), "hello".to_string())]);
    }

    #[tokio::test]
    #[ignore]
    async fn does_not_receive_notifications_on_channels_never_listened_to() {
        use std::sync::{Arc, Mutex};
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        let _listener = PgListener::connect(&test_dsn(), move |n| {
            received_clone.lock().unwrap().push(n.payload().to_string());
        })
        .await
        .unwrap();
        // Deliberately never call `.listen(...)`.

        let notifier = crate::PgPool::connect(&test_dsn(), 1).await.unwrap();
        notifier.query_raw("NOTIFY pgcon_listener_unheard_test, 'should not arrive'").await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(received.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn unlisten_stops_further_notifications() {
        use std::sync::{Arc, Mutex};
        let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let count_clone = count.clone();

        let listener = PgListener::connect(&test_dsn(), move |_n| {
            *count_clone.lock().unwrap() += 1;
        })
        .await
        .unwrap();
        listener.listen("pgcon_listener_unlisten_test").await.unwrap();

        let notifier = crate::PgPool::connect(&test_dsn(), 1).await.unwrap();
        notifier.query_raw("NOTIFY pgcon_listener_unlisten_test, 'first'").await.unwrap();
        for _ in 0..50 {
            if *count.lock().unwrap() >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(*count.lock().unwrap(), 1);

        listener.unlisten("pgcon_listener_unlisten_test").await.unwrap();
        notifier.query_raw("NOTIFY pgcon_listener_unlisten_test, 'second'").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(*count.lock().unwrap(), 1, "unlisten should have stopped further notifications");
    }

    #[tokio::test]
    #[ignore]
    async fn queries_run_on_the_same_connection_as_listen() {
        let listener = PgListener::connect(&test_dsn(), |_n| {}).await.unwrap();
        listener.execute_typed("CREATE TEMP TABLE pgcon_listener_query_test (id int8)", &[]).await.unwrap();
        listener
            .execute_typed("INSERT INTO pgcon_listener_query_test (id) VALUES ($1::int8)", &[CachedValue::I64(7)])
            .await
            .unwrap();
        let rows = listener
            .query_typed("SELECT (id) AS result FROM pgcon_listener_query_test", &[], &ExtensionOids::default())
            .await
            .unwrap();
        assert_eq!(rows, vec![CachedValue::I64(7)]);
    }
}
