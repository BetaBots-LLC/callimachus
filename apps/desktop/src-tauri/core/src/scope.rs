//! How a `project` argument selects threads: one implementation of the SQL condition and of
//! the same match in Rust, so the two cannot disagree.
//!
//! A thread's project is its canonical key, `COALESCE(project_key, project_path)`. `cal -p` and
//! MCP's `project` take a repo name or a path fragment ([`ProjectScope::Fragment`], a
//! substring, as SQLite's `LIKE` matches: ASCII case-insensitive); the app's chips and Project
//! page hold the exact key `list_projects` returned ([`ProjectScope::Exact`]). A fragment's
//! `_` and `%` are literal: unescaped, `-p my_app` also matched `myXapp` and `-p 100%` every
//! project starting `100`, while the Rust side (`issues`) compared literally.

use rusqlite::ToSql;

use crate::search::ProjectMatch;

/// What a project argument scopes a read to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectScope {
    /// Every project.
    Any,
    /// Exactly this canonical key.
    Exact(String),
    /// Any key containing this text (ASCII case-insensitive, `_` and `%` literal).
    Fragment(String),
}

impl ProjectScope {
    /// A substring scope for a `cal` / MCP `project` argument; unset or blank is [`Any`].
    ///
    /// [`Any`]: ProjectScope::Any
    pub fn fragment(project: Option<&str>) -> Self {
        match project.filter(|p| !p.is_empty()) {
            Some(p) => Self::Fragment(p.to_string()),
            None => Self::Any,
        }
    }

    /// `project` matched as `mode` says; the caller has already dropped a blank one (an empty
    /// fragment still matches every keyed thread, as `LIKE '%%'` did).
    pub fn with_match(project: &str, mode: ProjectMatch) -> Self {
        match mode {
            ProjectMatch::Exact => Self::Exact(project.to_string()),
            ProjectMatch::Substring => Self::Fragment(project.to_string()),
        }
    }

    /// The bind value for [`Self::condition_at`]: the key itself, or the fragment escaped
    /// into `%fragment%`. None for [`ProjectScope::Any`].
    pub fn value(&self) -> Option<String> {
        match self {
            Self::Any => None,
            Self::Exact(key) => Some(key.clone()),
            Self::Fragment(f) => Some(like_contains(f)),
        }
    }

    /// The condition (no leading AND) on `{t}` (a threads alias) with its value bound at
    /// `?{ph}`, or None for [`ProjectScope::Any`]. Exact is spelled as an OR on the columns,
    /// not `COALESCE(..) =`, so idx_threads_project_key / idx_threads_project stay usable when
    /// a plan wants them; the two forms select the same rows.
    pub fn condition_at(&self, t: &str, ph: usize) -> Option<String> {
        match self {
            Self::Any => None,
            Self::Exact(_) => Some(format!(
                "({t}.project_key = ?{ph} OR ({t}.project_key IS NULL AND {t}.project_path = ?{ph}))"
            )),
            Self::Fragment(_) => Some(format!(
                "COALESCE({t}.project_key, {t}.project_path) LIKE ?{ph} ESCAPE '\\'"
            )),
        }
    }

    /// The condition on `{t}` with its value pushed onto `args`, or None (nothing pushed) for
    /// [`ProjectScope::Any`].
    pub fn sql(&self, t: &str, args: &mut Vec<Box<dyn ToSql>>) -> Option<String> {
        let value = self.value()?;
        args.push(Box::new(value));
        self.condition_at(t, args.len())
    }

    /// Whether a stored canonical key is in scope, as [`Self::sql`] selects it.
    pub fn matches(&self, key: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(k) => key == k,
            // SQLite's LIKE folds ASCII case only, so this does too.
            Self::Fragment(f) => key.to_ascii_lowercase().contains(&f.to_ascii_lowercase()),
        }
    }
}

/// `s` as a `LIKE ... ESCAPE '\'` pattern matching it anywhere, with `%`, `_` and the escape
/// character itself escaped.
pub(crate) fn like_contains(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Threads keyed so that an unescaped `_` or `%` would match the wrong ones.
    fn keyed_threads() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id INTEGER PRIMARY KEY, project_key TEXT, project_path TEXT);
             INSERT INTO threads (project_key, project_path) VALUES
               ('/r/my_app', '/r/my_app'),
               ('/r/myXapp', '/r/myXapp'),
               ('/r/50%off', '/r/50%off'),
               ('/r/50Xoff', '/r/50Xoff'),
               (NULL, 'C:\\Users\\me\\repo'),
               (NULL, 'C:\\Users\\me\\repoX');",
        )
        .unwrap();
        conn
    }

    fn select(conn: &Connection, scope: &ProjectScope) -> Vec<String> {
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let cond = scope.sql("t", &mut args).unwrap_or_else(|| "1".into());
        let sql = format!(
            "SELECT COALESCE(t.project_key, t.project_path) FROM threads t WHERE {cond} ORDER BY t.id"
        );
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).unwrap();
        stmt.query_map(refs.as_slice(), |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn all_keys(conn: &Connection) -> Vec<String> {
        select(conn, &ProjectScope::Any)
    }

    /// `_`, `%` and `\` in a fragment match only themselves, in SQL and in `matches`, which
    /// agree on every key.
    #[test]
    fn fragment_wildcards_are_literal() {
        let conn = keyed_threads();
        for (fragment, want) in [
            ("my_app", vec!["/r/my_app"]),
            ("MY_APP", vec!["/r/my_app"]),
            ("50%", vec!["/r/50%off"]),
            ("repo", vec!["C:\\Users\\me\\repo", "C:\\Users\\me\\repoX"]),
            ("me\\repoX", vec!["C:\\Users\\me\\repoX"]),
        ] {
            let scope = ProjectScope::fragment(Some(fragment));
            assert_eq!(select(&conn, &scope), want, "{fragment}");
            let matched: Vec<String> = all_keys(&conn)
                .into_iter()
                .filter(|k| scope.matches(k))
                .collect();
            assert_eq!(matched, want, "matches({fragment})");
        }
    }

    #[test]
    fn exact_and_any_scopes() {
        let conn = keyed_threads();
        let exact = ProjectScope::with_match("/r/my_app", ProjectMatch::Exact);
        assert_eq!(select(&conn, &exact), ["/r/my_app"]);
        assert!(exact.matches("/r/my_app") && !exact.matches("/r/my_app/sub"));
        // A NULL key falls back to the path.
        let path = ProjectScope::with_match("C:\\Users\\me\\repo", ProjectMatch::Exact);
        assert_eq!(select(&conn, &path), ["C:\\Users\\me\\repo"]);

        assert_eq!(ProjectScope::fragment(None), ProjectScope::Any);
        assert_eq!(ProjectScope::fragment(Some("")), ProjectScope::Any);
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        assert!(ProjectScope::Any.sql("t", &mut args).is_none());
        assert!(args.is_empty(), "Any binds nothing");
        assert_eq!(all_keys(&conn).len(), 6);
        assert!(ProjectScope::Any.matches("anything"));
    }
}
