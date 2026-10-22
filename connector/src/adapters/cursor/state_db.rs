//! Bounded, read-only access to Cursor's live SQLite state without a system sqlite3 executable.

use rusqlite::{types::ValueRef, Connection, InterruptHandle, OpenFlags};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(2);
const MAX_RESULT: usize = 64 * 1024;
const MAX_ROWS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DbError {
    Open,
    Query,
    TimedOut,
    TooLarge,
    InvalidValue,
}

fn query_blocking(
    database: &Path,
    sql: &str,
    interrupt: &Arc<Mutex<Option<InterruptHandle>>>,
) -> Result<Vec<Value>, DbError> {
    let deadline = Instant::now() + DEADLINE;
    // Read-only is essential: Cursor may have a live WAL. Immutable mode would ignore recent WAL
    // entries, including a just-recorded join marker, so it is unsuitable for this live database.
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| DbError::Open)?;
    connection
        .busy_timeout(Duration::from_millis(100))
        .map_err(|_| DbError::Open)?;
    *interrupt.lock().map_err(|_| DbError::Open)? = Some(connection.get_interrupt_handle());
    connection
        .progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(|_| DbError::Open)?;
    let mut statement = connection.prepare(sql).map_err(|_| DbError::Query)?;
    let columns = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let mut rows = statement.query([]).map_err(|_| DbError::Query)?;
    let mut result = Vec::new();
    let mut size = 0usize;
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(_) if Instant::now() >= deadline => return Err(DbError::TimedOut),
            Err(_) => return Err(DbError::Query),
        };
        if Instant::now() >= deadline {
            return Err(DbError::TimedOut);
        }
        if result.len() >= MAX_ROWS {
            return Err(DbError::TooLarge);
        }
        let mut item = Map::new();
        for (index, name) in columns.iter().enumerate() {
            let value = match row.get_ref(index).map_err(|_| DbError::InvalidValue)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => Value::from(value),
                ValueRef::Real(value) => Value::from(value),
                ValueRef::Text(bytes) => {
                    size = size.checked_add(bytes.len()).ok_or(DbError::TooLarge)?;
                    if size > MAX_RESULT {
                        return Err(DbError::TooLarge);
                    }
                    Value::from(std::str::from_utf8(bytes).map_err(|_| DbError::InvalidValue)?)
                }
                ValueRef::Blob(_) => return Err(DbError::InvalidValue),
            };
            size = size.checked_add(name.len()).ok_or(DbError::TooLarge)?;
            if size > MAX_RESULT {
                return Err(DbError::TooLarge);
            }
            item.insert(name.clone(), value);
        }
        result.push(Value::Object(item));
    }
    Ok(result)
}

/// SQLite work runs away from the runtime worker. Timeout interrupts a live query and awaits
/// its exit; the progress callback and short busy timeout also bound work after cancellation.
pub(super) async fn query(database: &Path, sql: &str) -> Result<Vec<Value>, DbError> {
    let database = PathBuf::from(database);
    let sql = sql.to_owned();
    let interrupt = Arc::new(Mutex::new(None));
    let handle = Arc::clone(&interrupt);
    let mut task = tokio::task::spawn_blocking(move || query_blocking(&database, &sql, &handle));
    match tokio::time::timeout(DEADLINE + Duration::from_millis(200), &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(DbError::Query),
        Err(_) => {
            if let Ok(guard) = interrupt.lock() {
                if let Some(handle) = guard.as_ref() {
                    handle.interrupt();
                }
            }
            let _ = tokio::time::timeout(Duration::from_millis(300), task).await;
            Err(DbError::TimedOut)
        }
    }
}

#[cfg(test)]
pub(super) fn fixture(database: &Path, sql: &str) {
    Connection::open(database)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_only_query_does_not_create_missing_database_and_needs_no_cli() {
        let scratch = crate::test_support::temp_dir("cursor-state-db-read-only");
        let database = scratch.path().join("state.vscdb");
        assert_eq!(query(&database, "SELECT 1").await, Err(DbError::Open));
        assert!(!database.exists());
        fixture(&database, "CREATE TABLE cursorDiskKV(key TEXT, value TEXT); INSERT INTO cursorDiskKV VALUES ('one', 'two');");
        assert_eq!(
            query(&database, "SELECT value FROM cursorDiskKV")
                .await
                .unwrap(),
            vec![serde_json::json!({"value":"two"})]
        );
        assert!(!database.with_extension("vscdb-wal").exists());
        assert!(!database.with_extension("vscdb-shm").exists());
    }

    #[tokio::test]
    async fn long_query_is_interrupted_and_large_result_is_refused() {
        let scratch = crate::test_support::temp_dir("cursor-state-db-bounds");
        let database = scratch.path().join("state.vscdb");
        fixture(&database, "CREATE TABLE example(value TEXT);");
        let started = Instant::now();
        let expensive = "WITH RECURSIVE count(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM count) SELECT sum(x) FROM count";
        assert_eq!(query(&database, expensive).await, Err(DbError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            query(&database, "SELECT printf('%0100000d', 0) AS value").await,
            Err(DbError::TooLarge)
        );
    }
}
