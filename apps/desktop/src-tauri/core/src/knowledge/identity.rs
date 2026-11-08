//! Identity that survives machines and re-indexing: a random `uid` per fact (the merge key for
//! repo memory files and anything that syncs later), `source_seq` so a fact finds its source
//! message again after a full re-index, and a portable project id (the normalized `origin`
//! remote) so two machines that keep the same repo at different paths can agree on which
//! project a fact belongs to. The local `project_key` (the canonical absolute path) stays the
//! key for everything else; nothing is rekeyed.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// A new fact's `uid`: a random v4 UUID, lowercase and hyphenated (the same form 0034's
/// backfill and its insert trigger write).
pub fn new_fact_uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Point this thread's facts back at their source messages after its messages were replaced
/// (a full re-index deletes and reinserts every row, so the ids change and 0011's ON DELETE
/// SET NULL cleared `source_message_id`). A fact binds to the message at its `source_seq`
/// only when that message still contains the fact's text: a replace can shift the seqs (a
/// parser that starts storing rows it used to skip, as Codex's `apply_patch` calls did), and
/// whatever sits at the old seq then is another message. Otherwise it binds to the nearest
/// message in the thread that contains the text (and takes its seq), else stays NULL (a fact
/// the user reworded no longer quotes its source). Returns how many facts were rebound.
pub fn rebind_fact_sources(conn: &Connection, thread_id: i64) -> Result<usize> {
    let facts: Vec<(i64, String, i64)> = {
        let mut stmt = conn.prepare_cached(UNBOUND_SQL)?;
        let rows = stmt
            .query_map([thread_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut at_seq = conn.prepare_cached(AT_SEQ_SQL)?;
    let mut nearest = conn.prepare_cached(NEAREST_SQL)?;
    let mut bind = conn
        .prepare_cached("UPDATE facts SET source_message_id = ?2, source_seq = ?3 WHERE id = ?1")?;
    let mut n = 0;
    for (fact_id, text, seq) in facts {
        // A TODO capped at extraction ends in an ellipsis its message never had.
        let needle = text.trim().trim_end_matches('…').trim_end();
        if needle.is_empty() {
            continue;
        }
        let hit: Option<(i64, i64)> = at_seq
            .query_row(params![thread_id, seq, needle], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let hit = match hit {
            Some(h) => Some(h),
            None => nearest
                .query_row(params![thread_id, seq, needle], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?,
        };
        if let Some((message_id, found_seq)) = hit {
            bind.execute(params![fact_id, message_id, found_seq])?;
            n += 1;
        }
    }
    Ok(n)
}

/// The facts a replace left without a source, off idx_facts_thread.
pub(crate) const UNBOUND_SQL: &str = "SELECT id, text, source_seq FROM facts
     WHERE thread_id = ?1 AND source_seq IS NOT NULL AND source_message_id IS NULL";

/// The message at the fact's seq, if it still quotes the fact: one probe of the
/// (thread_id, seq) unique index.
pub(crate) const AT_SEQ_SQL: &str = "SELECT id, seq FROM messages
     WHERE thread_id = ?1 AND seq = ?2 AND instr(lower(text), lower(?3)) > 0";

/// The nearest message in the thread that quotes the fact: a walk of the thread's rows, run
/// only for a fact whose seq moved.
pub(crate) const NEAREST_SQL: &str = "SELECT id, seq FROM messages
     WHERE thread_id = ?1 AND instr(lower(text), lower(?3)) > 0
     ORDER BY abs(seq - ?2), seq LIMIT 1";

/// How a plain importer that already has a fact's uid (another machine) can write it: an
/// upsert on the uid, so a uid already present updates the local row instead of failing on
/// `idx_facts_uid` (a full, non-partial unique index, which is what lets ON CONFLICT name it).
/// The text follows the import unless the user edited or hid the local copy; the uid itself
/// is never updated (0034's `facts_uid_bu` refuses an empty one). Parameters: uid, thread_id,
/// kind, text, rationale, status, extractor, origin_ref, created_at.
///
/// Repo memory does not use it: an upsert would also rewrite a fact this machine wrote and
/// published (the local copy must win) and cannot see which project a uid belongs to. It
/// looks each uid up first and applies the same rule by hand (`repo_memory::import_repo`).
pub const IMPORT_FACT_SQL: &str = "INSERT INTO facts
        (uid, thread_id, kind, text, rationale, status, extractor, origin_ref, created_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
     ON CONFLICT(uid) DO UPDATE SET text = excluded.text, rationale = excluded.rationale
     WHERE facts.edited = 0 AND facts.hidden = 0";

/// How long a cached portable id is trusted before `git` is asked again (a remote can be
/// renamed or added; a day is plenty for something that changes that rarely).
pub const PORTABLE_ID_TTL_SECS: i64 = 24 * 3600;

/// What `git` says about a project directory: its `origin` URL and where the key sits below
/// the checkout's toplevel ("" at the toplevel).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteProbe {
    pub remote: Option<String>,
    pub subpath: String,
}

/// The project's portable id: the normalized `origin` remote (`github.com/a/b`), plus
/// `#<subpath>` when `key` lies below the checkout's toplevel. None for a project with no
/// remote (or no checkout on this machine): callers fall back to the local key. Cached in
/// `projects` and recomputed after [`PORTABLE_ID_TTL_SECS`]; on a read-only connection (or an
/// index without the table yet) it is computed and not stored.
pub fn portable_project_id(conn: &Connection, key: &str) -> Option<String> {
    portable_project_id_with(
        conn,
        key,
        chrono::Utc::now().timestamp(),
        false,
        probe_remote,
    )
}

/// [`portable_project_id`] that skips the cache: after the user changed the remote, or when a
/// caller (publishing repo memory) needs the current answer.
pub fn refresh_portable_project_id(conn: &Connection, key: &str) -> Option<String> {
    portable_project_id_with(
        conn,
        key,
        chrono::Utc::now().timestamp(),
        true,
        probe_remote,
    )
}

/// [`portable_project_id`] with the clock, the cache bypass and the `git` probe injected.
pub(crate) fn portable_project_id_with(
    conn: &Connection,
    key: &str,
    now: i64,
    force: bool,
    probe: impl FnOnce(&str) -> Option<RemoteProbe>,
) -> Option<String> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    if !force {
        let cached: Option<(Option<String>, i64)> = conn
            .query_row(
                "SELECT portable_id, COALESCE(refreshed_at, 0) FROM projects WHERE key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        if let Some((id, at)) = cached {
            if now - at < PORTABLE_ID_TTL_SECS {
                return id;
            }
        }
    }
    // A failed probe (git missing from the GUI's PATH, a `safe.directory` refusal, a checkout
    // not cloned on this machine yet, a spawn error) is not "no remote": caching it would hide
    // the project for a day. Answer from a stale cached id if there is one, store nothing,
    // and ask git again next time.
    let Some(probed) = probe(key) else {
        return conn
            .query_row(
                "SELECT portable_id FROM projects WHERE key = ?1",
                [key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten();
    };
    let id = portable_id(probed.remote.as_deref(), &probed.subpath);
    // A read-only connection (cal, a hook) or an index from before 0034 cannot store it:
    // the answer is still right, it is just computed again next time.
    let _ = conn.execute(
        "INSERT INTO projects (key, remote, subpath, portable_id, refreshed_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(key) DO UPDATE SET remote = excluded.remote, subpath = excluded.subpath,
            portable_id = excluded.portable_id, refreshed_at = excluded.refreshed_at",
        params![
            key,
            probed.remote,
            (!probed.subpath.is_empty()).then_some(probed.subpath.as_str()),
            id,
            now
        ],
    );
    id
}

/// The local project keys whose cached portable id is `id`, sorted. Only keys some caller
/// already resolved through [`portable_project_id`] are known (the watcher does it for every
/// checkout it links commits in); an unknown id gives an empty list.
pub fn local_keys_for_portable(conn: &Connection, id: &str) -> Vec<String> {
    let run = || -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare_cached(LOCAL_KEYS_SQL)?;
        let rows = stmt
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    };
    run().unwrap_or_default()
}

/// Off idx_projects_portable.
pub(crate) const LOCAL_KEYS_SQL: &str =
    "SELECT key FROM projects WHERE portable_id = ?1 ORDER BY key";

/// `remote` normalized, plus `#subpath` when the project sits below the toplevel.
fn portable_id(remote: Option<&str>, subpath: &str) -> Option<String> {
    let base = normalize_remote(remote?)?;
    let sub = subpath.trim_matches('/');
    Some(if sub.is_empty() {
        base
    } else {
        format!("{base}#{sub}")
    })
}

/// A git remote URL as `host/path`: scheme, user (and password) and port dropped, the host
/// lowercased, a trailing `.git` and slashes trimmed, so `ssh://git@github.com/a/b.git`,
/// `git@github.com:a/b` and `https://GitHub.com/a/b/` are all `github.com/a/b`. On the hosts
/// whose repository paths ignore case ([`CASE_INSENSITIVE_HOSTS`]) the path is lowercased
/// too: `github.com/Me/App` and `github.com/me/app` are one repo. None for a local path or
/// `file://` remote, which names a directory on one machine only.
pub fn normalize_remote(url: &str) -> Option<String> {
    let u = url.trim();
    let (authority, path) = if let Some(i) = u.find("://") {
        if u[..i].eq_ignore_ascii_case("file") {
            return None;
        }
        let rest = &u[i + 3..];
        let slash = rest.find('/')?;
        (&rest[..slash], &rest[slash + 1..])
    } else {
        // scp-like `[user@]host:path`. A colon after the first slash is a local path, and a
        // one-letter "host" is a Windows drive (`C:\repo`).
        let colon = u.find(':')?;
        if u.find('/').is_some_and(|s| s < colon) || colon <= 1 {
            return None;
        }
        (&u[..colon], &u[colon + 1..])
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    if CASE_INSENSITIVE_HOSTS.contains(&host.as_str()) {
        return Some(format!("{host}/{}", path.to_ascii_lowercase()));
    }
    Some(format!("{host}/{path}"))
}

/// Hosts that resolve owner and repository names without regard to case. A self-hosted
/// GitLab or Gitea may not, so its path keeps the case the remote was written in.
pub const CASE_INSENSITIVE_HOSTS: &[&str] = &["github.com", "gitlab.com", "bitbucket.org"];

/// Ask git for `dir`'s `origin` URL and its toplevel. None when `dir` is not a checkout on
/// this machine (moved, deleted, never cloned here).
fn probe_remote(dir: &str) -> Option<RemoteProbe> {
    let path = crate::gitlink::plain_path(dir);
    if !std::path::Path::new(&path).is_dir() {
        return None;
    }
    // Bounded like the watcher's probe: a hung git (a network mount, a credential prompt)
    // must not stall a publish or a hook. Both commands share one 3 s deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let git = |args: &[&str]| -> Option<String> {
        crate::gitlink::run_git_until(&path, args, deadline)
            .ok()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let toplevel = git(&["rev-parse", "--show-toplevel"])?;
    let remote = git(&["remote", "get-url", "origin"]);
    Some(RemoteProbe {
        remote,
        subpath: subpath_below(&toplevel, &path),
    })
}

/// `dir` relative to `toplevel` with `/` separators, "" when they are the same directory or
/// `dir` is not below it. Both sides are canonicalized when they exist (git prints a resolved
/// toplevel; `/tmp` vs `/private/tmp` on macOS).
fn subpath_below(toplevel: &str, dir: &str) -> String {
    let canon = |p: &str| {
        std::fs::canonicalize(p)
            .map(|c| crate::gitlink::plain_path(&c.to_string_lossy()))
            .unwrap_or_else(|_| p.to_string())
            .replace('\\', "/")
    };
    let (top, dir) = (canon(toplevel), canon(dir));
    let top = top.trim_end_matches('/');
    dir.strip_prefix(top)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|rest| rest.trim_matches('/').to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Connection {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "calli_ident_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).unwrap()
    }

    fn plan(conn: &Connection, sql: &str, arg: impl rusqlite::ToSql) -> String {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([arg], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ")
    }

    #[test]
    fn remotes_normalize_to_one_id() {
        let want = Some("github.com/a/b".to_string());
        for url in [
            "ssh://git@github.com/a/b.git",
            "git@github.com:a/b",
            "git@github.com:a/b.git",
            "https://github.com/a/b",
            "https://user:token@GitHub.com/a/b.git/",
            "ssh://git@github.com:22/a/b",
            "git://github.com/a/b.git",
        ] {
            assert_eq!(normalize_remote(url), want, "{url}");
        }
        assert_eq!(
            normalize_remote("https://gitlab.example.com/Group/sub/Repo.git"),
            Some("gitlab.example.com/Group/sub/Repo".to_string()),
            "a self-hosted path keeps its case"
        );
        // GitHub, GitLab and Bitbucket ignore case in owner and repo names.
        for (url, want) in [
            ("https://github.com/Me/App.git", "github.com/me/app"),
            ("git@github.com:me/app", "github.com/me/app"),
            (
                "git@GitLab.com:Group/Sub/Repo.git",
                "gitlab.com/group/sub/repo",
            ),
            ("https://bitbucket.org/Team/Repo", "bitbucket.org/team/repo"),
        ] {
            assert_eq!(normalize_remote(url).as_deref(), Some(want), "{url}");
        }
        // Local remotes name a directory on one machine.
        for url in [
            "/srv/git/repo.git",
            "file:///srv/git/repo",
            "C:\\repos\\x",
            "../x",
            "",
        ] {
            assert_eq!(normalize_remote(url), None, "{url}");
        }
    }

    #[test]
    fn portable_id_caches_and_refreshes_after_a_day() {
        let conn = temp_db();
        let probe = |remote: &str, sub: &str| RemoteProbe {
            remote: Some(remote.to_string()),
            subpath: sub.to_string(),
        };
        let mut calls = 0;
        let id = portable_project_id_with(&conn, "/w/app", 1_000, false, |_| {
            calls += 1;
            Some(probe("git@github.com:me/app.git", ""))
        });
        assert_eq!(id.as_deref(), Some("github.com/me/app"));
        // Cached: the probe is not asked again inside the TTL.
        let again = portable_project_id_with(&conn, "/w/app", 2_000, false, |_| {
            calls += 1;
            None
        });
        assert_eq!(again.as_deref(), Some("github.com/me/app"));
        assert_eq!(calls, 1);
        // Past the TTL (or forced) it is recomputed and stored.
        let moved =
            portable_project_id_with(&conn, "/w/app", 1_000 + PORTABLE_ID_TTL_SECS, false, |_| {
                Some(probe("https://github.com/me/app-renamed", ""))
            });
        assert_eq!(moved.as_deref(), Some("github.com/me/app-renamed"));
        // A subdirectory project carries its subpath; no remote caches None.
        let sub = portable_project_id_with(&conn, "/w/mono/web", 5_000, true, |_| {
            Some(probe("git@github.com:me/mono.git", "web"))
        });
        assert_eq!(sub.as_deref(), Some("github.com/me/mono#web"));
        assert_eq!(
            portable_project_id_with(&conn, "/w/local", 5_000, false, |_| Some(RemoteProbe {
                remote: None,
                subpath: String::new()
            })),
            None
        );
        let none_cached = portable_project_id_with(&conn, "/w/local", 6_000, false, |_| {
            panic!("a cached None is not probed again")
        });
        assert_eq!(none_cached, None);
        // A failed probe (git missing, the checkout not cloned here yet) is not cached: the
        // next call asks again, and a stale cached id still answers meanwhile.
        assert_eq!(
            portable_project_id_with(&conn, "/w/later", 5_000, false, |_| None),
            None
        );
        let mut asked = false;
        let later = portable_project_id_with(&conn, "/w/later", 5_001, false, |_| {
            asked = true;
            Some(probe("git@github.com:me/later.git", ""))
        });
        assert!(asked, "a failed probe was not cached");
        assert_eq!(later.as_deref(), Some("github.com/me/later"));
        let stale = portable_project_id_with(
            &conn,
            "/w/later",
            5_001 + PORTABLE_ID_TTL_SECS,
            false,
            |_| None,
        );
        assert_eq!(stale.as_deref(), Some("github.com/me/later"));
        // Two checkouts of one repo map back to both local keys.
        portable_project_id_with(&conn, "/other/app", 5_000, true, |_| {
            Some(probe("ssh://git@github.com/me/app-renamed.git", ""))
        });
        assert_eq!(
            local_keys_for_portable(&conn, "github.com/me/app-renamed"),
            vec!["/other/app".to_string(), "/w/app".to_string()]
        );
        assert!(local_keys_for_portable(&conn, "github.com/nobody/x").is_empty());
        assert!(plan(&conn, LOCAL_KEYS_SQL, "x").contains("idx_projects_portable"));
    }

    #[test]
    fn a_real_checkout_reports_its_remote_and_subpath() {
        let dir = std::env::temp_dir().join(format!("calli_ident_git_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg/web")).unwrap();
        let git = |args: &[&str]| {
            crate::gitlink::git_command()
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // no git on this machine
        }
        assert!(git(&[
            "remote",
            "add",
            "origin",
            "git@github.com:me/mono.git"
        ]));
        let sub = dir.join("pkg/web");
        let p = probe_remote(&sub.to_string_lossy()).expect("a checkout");
        assert_eq!(p.remote.as_deref(), Some("git@github.com:me/mono.git"));
        assert_eq!(p.subpath, "pkg/web");
        let conn = temp_db();
        assert_eq!(
            refresh_portable_project_id(&conn, &sub.to_string_lossy()).as_deref(),
            Some("github.com/me/mono#pkg/web")
        );
        assert_eq!(probe_remote("/no/such/dir/anywhere"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_upserts_on_the_uid() {
        let conn = temp_db();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 'imp', 'imp')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        let uid = new_fact_uid();
        let import = |text: &str| {
            conn.execute(
                IMPORT_FACT_SQL,
                params![
                    uid,
                    tid,
                    "decision",
                    text,
                    None::<String>,
                    "open",
                    "repo",
                    None::<String>,
                    1
                ],
            )
            .unwrap();
        };
        import("Use SQLite");
        import("Use SQLite, WAL mode");
        let rows = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT text FROM facts WHERE uid = ?1")
                .unwrap()
                .query_map([&uid], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(rows(&conn), vec!["Use SQLite, WAL mode".to_string()]);
        // The user's own edit wins over a later import.
        conn.execute(
            "UPDATE facts SET text = 'mine', edited = 1 WHERE uid = ?1",
            [&uid],
        )
        .unwrap();
        import("theirs");
        assert_eq!(rows(&conn), vec!["mine".to_string()]);
    }

    #[test]
    fn uids_are_v4_and_unique() {
        let a = new_fact_uid();
        let b = new_fact_uid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "{a}");
        assert_eq!(a, a.to_ascii_lowercase());
    }

    #[test]
    fn every_fact_gets_a_uid_and_rebinds_after_a_full_reindex() {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let mut conn = temp_db();
        // An insert that names no uid (a fixture, an older path) still gets one.
        let sid = source_id(&conn, "claude_code").unwrap();
        let msg = |text: &str| ParsedMessage {
            role: "user".into(),
            text: text.into(),
            tool_name: None,
            ts: Some(100),
        };
        let mut thread = ParsedThread {
            external_id: "t1".into(),
            messages: vec![msg("hello"), msg("TODO: wire the exporter"), msg("bye")],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let tid: i64 = conn
            .query_row("SELECT id FROM threads WHERE external_id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO facts (thread_id, kind, text, created_at) VALUES (?1, 'gotcha', 'g', 1)",
            [tid],
        )
        .unwrap();
        let nulls: i64 = conn
            .query_row("SELECT COUNT(*) FROM facts WHERE uid IS NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(nulls, 0, "the trigger fills a missing uid");
        // An empty uid is missing too (two of them would collide on the unique index), and
        // no update may clear one.
        for _ in 0..2 {
            conn.execute(
                "INSERT INTO facts (thread_id, kind, text, created_at, uid)
                 VALUES (?1, 'gotcha', 'e', 1, '')",
                [tid],
            )
            .unwrap();
        }
        let empties: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM facts WHERE uid = '' OR length(uid) <> 36",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(empties, 0);
        for clear in ["NULL", "''"] {
            let err = conn
                .execute(
                    &format!("UPDATE facts SET uid = {clear} WHERE text = 'g'"),
                    [],
                )
                .unwrap_err();
            assert!(err.to_string().contains("cannot be cleared"), "{err}");
        }

        // The heuristic todo points at seq 1. The user closes it, so a re-index keeps it.
        let (fid, uid, seq, src): (i64, String, i64, i64) = conn
            .query_row(
                "SELECT id, uid, source_seq, source_message_id FROM facts WHERE kind = 'todo'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(seq, 1);
        crate::knowledge::set_todo_done(&conn, fid, true).unwrap();

        // Another thread's rows sit above t1's, so the replace cannot reuse t1's rowids.
        let other = ParsedThread {
            external_id: "t2".into(),
            messages: vec![msg("other")],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &other).unwrap();
        // An in-place edit of message 0 forces a full replace: every message id changes.
        thread.messages[0] = msg("hello, edited");
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let (uid2, src2): (String, Option<i64>) = conn
            .query_row(
                "SELECT uid, source_message_id FROM facts WHERE id = ?1",
                [fid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(uid2, uid, "the uid survives");
        let new_src: i64 = conn
            .query_row(
                "SELECT id FROM messages WHERE thread_id = ?1 AND seq = 1",
                [tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(new_src, src, "the messages were rewritten");
        assert_eq!(
            src2,
            Some(new_src),
            "the closed todo is bound to the new row"
        );

        let p = plan(&conn, UNBOUND_SQL, tid);
        assert!(p.contains("idx_facts_"), "{p}");
        assert!(!p.contains("SCAN"), "{p}");
        let p = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {AT_SEQ_SQL}"))
            .unwrap()
            .query_map(params![tid, 1, "x"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");
        assert!(p.contains("seq=?"), "{p}");
        assert!(!p.contains("SCAN"), "{p}");
        let p = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {NEAREST_SQL}"))
            .unwrap()
            .query_map(params![tid, 1, "x"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");
        assert!(p.contains("thread_id=?"), "the thread's rows only: {p}");
    }

    #[test]
    fn a_shifted_thread_rebinds_by_text_not_by_position() {
        use crate::indexer::{source_id, upsert_thread, ParsedMessage, ParsedThread};
        let mut conn = temp_db();
        let sid = source_id(&conn, "codex").unwrap();
        let msg = |role: &str, text: &str, tool: Option<&str>| ParsedMessage {
            role: role.into(),
            text: text.into(),
            tool_name: tool.map(str::to_string),
            ts: Some(100),
        };
        let mut thread = ParsedThread {
            external_id: "shift".into(),
            messages: vec![
                msg("user", "start", None),
                msg("assistant", "TODO: wire the exporter", None),
                msg("user", "TODO: ship the docs", None),
            ],
            ..Default::default()
        };
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let fact = |conn: &Connection, text: &str| -> (i64, i64) {
            conn.query_row(
                "SELECT id, source_seq FROM facts WHERE text = ?1",
                [text],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let (wire, seq) = fact(&conn, "wire the exporter");
        assert_eq!(seq, 1);
        crate::knowledge::set_todo_done(&conn, wire, true).unwrap();
        let (_, docs_seq) = fact(&conn, "ship the docs");
        assert_eq!(docs_seq, 2);

        // The re-parse stores two rows it used to skip ahead of them (an apply_patch call
        // and its output): every later seq moves by two.
        thread
            .messages
            .insert(1, msg("assistant", "apply_patch: x", Some("apply_patch")));
        thread.messages.insert(2, msg("tool", "Success.", None));
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let bound = |conn: &Connection, id: i64| -> (Option<String>, Option<i64>) {
            conn.query_row(
                "SELECT m.text, f.source_seq FROM facts f
                 LEFT JOIN messages m ON m.id = f.source_message_id WHERE f.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            bound(&conn, wire),
            (Some("TODO: wire the exporter".to_string()), Some(3)),
            "the done TODO follows its message to seq 3, not the patch at seq 1"
        );

        // The user rewords the open TODO, then a replace that moves nothing (an in-place
        // edit of the first message) re-derives the thread.
        let (docs, docs_seq) = fact(&conn, "ship the docs");
        assert_eq!(docs_seq, 4);
        crate::knowledge::edit_fact(&conn, docs, "ship the docs site").unwrap();
        thread.messages[0] = msg("user", "start, edited", None);
        upsert_thread(&mut conn, sid, &thread).unwrap();
        let docs_copies: Vec<(String, i64)> = conn
            .prepare(
                "SELECT text, edited FROM facts WHERE kind = 'todo'
                 AND lower(text) LIKE 'ship the docs%' ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            docs_copies,
            vec![("ship the docs site".to_string(), 1)],
            "the reworded TODO is kept, and its original does not come back beside it"
        );
        // It quotes nothing in the thread any more: no source beats a wrong one.
        assert_eq!(bound(&conn, docs), (None, Some(4)));
        assert_eq!(bound(&conn, wire).1, Some(3));
    }
}
