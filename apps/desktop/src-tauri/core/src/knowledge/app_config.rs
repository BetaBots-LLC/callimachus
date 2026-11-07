//! The `app_config` key/value table in index.db: settings and one-shot markers that the
//! app, `cal` and the MCP server all read from the same store. Every setting goes through
//! these helpers so it is written, cleared and read back the same way everywhere (one
//! upsert, one "unset" meaning) instead of each module carrying its own SQL.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// The stored value for `key`, or None when it is unset.
pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM app_config WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

/// The value for `key` with surrounding whitespace trimmed, or None when it is unset or
/// blank. For free-text settings (paths, URLs, model names) where "" means "not set".
pub fn get_nonblank(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(get(conn, key)?
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty()))
}

/// Whether the flag `key` is on. Flags are stored as "1"; unset or anything else is off.
pub fn get_flag(conn: &Connection, key: &str) -> Result<bool> {
    Ok(get(conn, key)?.as_deref() == Some("1"))
}

/// Set `key` to `value`, inserting it or overwriting the previous value.
pub fn put(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO app_config (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Store a flag as "1" / "0" (read back with [`get_flag`]).
pub fn put_flag(conn: &Connection, key: &str, on: bool) -> Result<()> {
    put(conn, key, if on { "1" } else { "0" })
}

/// Remove `key`, so it reads as unset. A no-op when it was never stored.
pub fn delete(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM app_config WHERE key = ?1", [key])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE app_config (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        c
    }

    #[test]
    fn put_overwrites_and_delete_unsets() {
        let c = conn();
        assert_eq!(get(&c, "k").unwrap(), None);
        put(&c, "k", "one").unwrap();
        put(&c, "k", "two").unwrap();
        assert_eq!(get(&c, "k").unwrap().as_deref(), Some("two"));
        delete(&c, "k").unwrap();
        assert_eq!(get(&c, "k").unwrap(), None);
        // Deleting an unset key is fine.
        delete(&c, "k").unwrap();
    }

    #[test]
    fn nonblank_trims_and_treats_blank_as_unset() {
        let c = conn();
        put(&c, "p", "  /data/claude \n").unwrap();
        assert_eq!(
            get_nonblank(&c, "p").unwrap().as_deref(),
            Some("/data/claude")
        );
        put(&c, "p", "   ").unwrap();
        assert_eq!(get_nonblank(&c, "p").unwrap(), None);
        assert_eq!(get_nonblank(&c, "missing").unwrap(), None);
    }

    #[test]
    fn flags_round_trip_and_default_off() {
        let c = conn();
        assert!(!get_flag(&c, "f").unwrap());
        put_flag(&c, "f", true).unwrap();
        assert!(get_flag(&c, "f").unwrap());
        put_flag(&c, "f", false).unwrap();
        assert!(!get_flag(&c, "f").unwrap());
        // Only "1" is on.
        put(&c, "f", "true").unwrap();
        assert!(!get_flag(&c, "f").unwrap());
    }
}
