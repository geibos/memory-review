//! SQLite storage for cards (proposals), their source notes and threads.
//!
//! All query functions take a plain `&Connection`/`&mut Connection` and are
//! meant to run inside [`Db::call`], which moves them off the async runtime.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::domain::{Action, Event, Status, ValidatedProposal, transition};

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

/// Another open card already holds one of the requested source notes.
#[derive(Debug, thiserror::Error)]
#[error("a source note is already claimed by another open card")]
pub struct ClaimConflict;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub permalink: String,
    pub title: String,
    pub content_hash: String,
    /// Raw note text the current draft was based on.
    pub original: String,
}

#[derive(Debug, Clone)]
pub struct ProposalRow {
    pub id: i64,
    pub action: Action,
    pub status: Status,
    pub target_dir: Option<String>,
    pub target_title: Option<String>,
    pub draft: Option<String>,
    pub tags: Vec<String>,
    pub rationale: String,
    pub version: i64,
    pub snoozed_until: Option<OffsetDateTime>,
    pub error: Option<String>,
    pub applied_permalink: Option<String>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub author: String,
    pub body: String,
    pub draft_version: i64,
    pub sent: bool,
    pub created_at: OffsetDateTime,
}

pub struct NewProposal {
    pub v: ValidatedProposal,
    pub sources: Vec<SourceRow>,
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub id: i64,
    pub action: Action,
    pub status: Status,
    pub title: String,
    pub target_dir: Option<String>,
    pub sources_count: i64,
    pub unsent: i64,
    pub has_error: bool,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy)]
pub enum QueueFilter {
    /// Everything that is not accepted or closed.
    Open,
    All,
    Status(Status),
}

const SCHEMA: &str = "
CREATE TABLE proposals(
  id INTEGER PRIMARY KEY, action TEXT NOT NULL, status TEXT NOT NULL,
  target_dir TEXT, target_title TEXT, draft TEXT, tags TEXT NOT NULL DEFAULT '[]',
  rationale TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 1, snoozed_until TEXT,
  error TEXT, applied_permalink TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE proposal_sources(
  proposal_id INTEGER NOT NULL REFERENCES proposals(id), permalink TEXT NOT NULL,
  title TEXT NOT NULL, content_hash TEXT NOT NULL, original TEXT NOT NULL,
  active INTEGER NOT NULL DEFAULT 1);
CREATE UNIQUE INDEX one_active_claim ON proposal_sources(permalink) WHERE active = 1;
CREATE INDEX sources_by_proposal ON proposal_sources(proposal_id);
CREATE TABLE messages(
  id INTEGER PRIMARY KEY, proposal_id INTEGER NOT NULL REFERENCES proposals(id),
  author TEXT NOT NULL, body TEXT NOT NULL, draft_version INTEGER NOT NULL,
  sent INTEGER NOT NULL, created_at TEXT NOT NULL);
CREATE INDEX messages_by_proposal ON messages(proposal_id);
";

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Db::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Db> {
        Db::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Db> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", 1)?;
        } else if version != 1 {
            bail!("unsupported database schema version {version}");
        }
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    /// Runs `f` on the blocking thread pool with exclusive access to the connection.
    ///
    /// cancel-safe: yes — the closure runs to completion on the blocking pool even
    /// if the caller is dropped; the caller only loses the result.
    pub async fn call<R, F>(&self, f: F) -> anyhow::Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> anyhow::Result<R> + Send + 'static,
    {
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            // A panic inside another closure must not wedge the whole service.
            let mut conn = inner.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut conn)
        })
        .await
        .context("database task panicked")?
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn fmt_time(t: OffsetDateTime) -> String {
    // Whole seconds in UTC, so stored timestamps compare correctly as strings.
    let t = t
        .to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .unwrap_or(t);
    // Formatting a UTC timestamp as RFC 3339 cannot fail for years 0..=9999.
    t.format(&Rfc3339).unwrap_or_default()
}

fn parse_time(s: &str) -> rusqlite::Result<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
}

fn insert_sources(
    tx: &rusqlite::Transaction,
    id: i64,
    sources: &[SourceRow],
) -> anyhow::Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO proposal_sources(proposal_id, permalink, title, content_hash, original)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for s in sources {
        if let Err(e) = stmt.execute(params![
            id,
            s.permalink,
            s.title,
            s.content_hash,
            s.original
        ]) {
            if is_unique_violation(&e) {
                return Err(ClaimConflict.into());
            }
            return Err(e.into());
        }
    }
    Ok(())
}

fn tags_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".into())
}

pub fn insert_proposal(c: &mut Connection, np: &NewProposal) -> anyhow::Result<i64> {
    let tx = c.transaction()?;
    let t = fmt_time(now());
    tx.execute(
        "INSERT INTO proposals(action, status, target_dir, target_title, draft, tags, rationale,
                               created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![
            np.v.action.as_str(),
            Status::Ready.as_str(),
            np.v.target_dir,
            np.v.target_title,
            np.v.draft,
            tags_json(&np.v.tags),
            np.v.rationale,
            t
        ],
    )?;
    let id = tx.last_insert_rowid();
    insert_sources(&tx, id, &np.sources)?;
    tx.commit()?;
    Ok(id)
}

pub fn update_draft(
    c: &mut Connection,
    id: i64,
    v: &ValidatedProposal,
    sources: &[SourceRow],
) -> anyhow::Result<()> {
    let tx = c.transaction()?;
    tx.execute(
        "DELETE FROM proposal_sources WHERE proposal_id = ?1 AND active = 1",
        params![id],
    )?;
    insert_sources(&tx, id, sources)?;
    let n = tx.execute(
        "UPDATE proposals SET action = ?2, target_dir = ?3, target_title = ?4, draft = ?5,
                tags = ?6, rationale = ?7, version = version + 1, applied_permalink = NULL,
                updated_at = ?8
         WHERE id = ?1",
        params![
            id,
            v.action.as_str(),
            v.target_dir,
            v.target_title,
            v.draft,
            tags_json(&v.tags),
            v.rationale,
            fmt_time(now())
        ],
    )?;
    if n != 1 {
        bail!("card {id} not found");
    }
    tx.commit()?;
    Ok(())
}

pub fn cas_status(c: &Connection, id: i64, from: Status, to: Status) -> anyhow::Result<bool> {
    let n = c.execute(
        "UPDATE proposals SET status = ?3, updated_at = ?4 WHERE id = ?1 AND status = ?2",
        params![id, from.as_str(), to.as_str(), fmt_time(now())],
    )?;
    Ok(n == 1)
}

pub fn set_error(c: &Connection, id: i64, err: Option<&str>) -> anyhow::Result<()> {
    c.execute(
        "UPDATE proposals SET error = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, err, fmt_time(now())],
    )?;
    Ok(())
}

pub fn set_applied_permalink(c: &Connection, id: i64, permalink: &str) -> anyhow::Result<()> {
    c.execute(
        "UPDATE proposals SET applied_permalink = ?2 WHERE id = ?1",
        params![id, permalink],
    )?;
    Ok(())
}

pub fn snooze(c: &Connection, id: i64, until: OffsetDateTime) -> anyhow::Result<bool> {
    let n = c.execute(
        "UPDATE proposals SET status = 'snoozed', snoozed_until = ?2, updated_at = ?3
         WHERE id = ?1 AND status = 'ready'",
        params![id, fmt_time(until), fmt_time(now())],
    )?;
    Ok(n == 1)
}

pub fn wake_snoozed(c: &Connection, at: OffsetDateTime) -> anyhow::Result<usize> {
    Ok(c.execute(
        "UPDATE proposals SET status = 'ready', snoozed_until = NULL
         WHERE status = 'snoozed' AND snoozed_until <= ?1",
        params![fmt_time(at)],
    )?)
}

pub fn release_sources(c: &Connection, id: i64) -> anyhow::Result<()> {
    c.execute(
        "UPDATE proposal_sources SET active = 0 WHERE proposal_id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn claimed_permalinks(c: &Connection) -> anyhow::Result<HashSet<String>> {
    let mut stmt = c.prepare("SELECT permalink FROM proposal_sources WHERE active = 1")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn bad_value(what: &str, v: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unknown {what} `{v}`").into(),
    )
}

fn parse_action(v: String) -> rusqlite::Result<Action> {
    Action::parse(&v).ok_or_else(|| bad_value("action", &v))
}

fn parse_status(v: String) -> rusqlite::Result<Status> {
    Status::parse(&v).ok_or_else(|| bad_value("status", &v))
}

pub fn get_proposal(c: &Connection, id: i64) -> anyhow::Result<Option<ProposalRow>> {
    Ok(c.query_row(
        "SELECT id, action, status, target_dir, target_title, draft, tags, rationale, version,
                snoozed_until, error, applied_permalink, updated_at
         FROM proposals WHERE id = ?1",
        params![id],
        |r| {
            let tags: String = r.get(6)?;
            Ok(ProposalRow {
                id: r.get(0)?,
                action: parse_action(r.get(1)?)?,
                status: parse_status(r.get(2)?)?,
                target_dir: r.get(3)?,
                target_title: r.get(4)?,
                draft: r.get(5)?,
                tags: serde_json::from_str(&tags).unwrap_or_default(),
                rationale: r.get(7)?,
                version: r.get(8)?,
                snoozed_until: r
                    .get::<_, Option<String>>(9)?
                    .map(|s| parse_time(&s))
                    .transpose()?,
                error: r.get(10)?,
                applied_permalink: r.get(11)?,
                updated_at: parse_time(&r.get::<_, String>(12)?)?,
            })
        },
    )
    .optional()?)
}

pub fn sources(c: &Connection, id: i64) -> anyhow::Result<Vec<SourceRow>> {
    let mut stmt = c.prepare(
        "SELECT permalink, title, content_hash, original FROM proposal_sources
         WHERE proposal_id = ?1 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![id], |r| {
        Ok(SourceRow {
            permalink: r.get(0)?,
            title: r.get(1)?,
            content_hash: r.get(2)?,
            original: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn list_queue(c: &Connection, filter: QueueFilter) -> anyhow::Result<Vec<QueueItem>> {
    let (cond, arg) = match filter {
        QueueFilter::Open => ("p.status NOT IN ('accepted', 'closed')", None),
        QueueFilter::All => ("1 = 1", None),
        QueueFilter::Status(s) => ("p.status = ?1", Some(s.as_str())),
    };
    let sql = format!(
        "SELECT p.id, p.action, p.status,
                COALESCE(p.target_title,
                         (SELECT s.title FROM proposal_sources s
                          WHERE s.proposal_id = p.id ORDER BY s.rowid LIMIT 1), ''),
                p.target_dir,
                (SELECT COUNT(*) FROM proposal_sources s WHERE s.proposal_id = p.id),
                (SELECT COUNT(*) FROM messages m
                 WHERE m.proposal_id = p.id AND m.sent = 0 AND m.author = 'human') AS unsent,
                p.error IS NOT NULL, p.updated_at
         FROM proposals p
         WHERE {cond}
         ORDER BY CASE
                    WHEN p.status = 'ready' AND unsent > 0 THEN 0
                    WHEN p.status = 'ready' THEN 1
                    WHEN p.status = 'stale' THEN 2
                    WHEN p.status = 'applying' THEN 3
                    WHEN p.status = 'agent_working' THEN 4
                    WHEN p.status = 'snoozed' THEN 5
                    ELSE 6
                  END,
                  p.updated_at DESC, p.id DESC"
    );
    let mut stmt = c.prepare(&sql)?;
    let map = |r: &rusqlite::Row| {
        Ok(QueueItem {
            id: r.get(0)?,
            action: parse_action(r.get(1)?)?,
            status: parse_status(r.get(2)?)?,
            title: r.get(3)?,
            target_dir: r.get(4)?,
            sources_count: r.get(5)?,
            unsent: r.get(6)?,
            has_error: r.get(7)?,
            updated_at: parse_time(&r.get::<_, String>(8)?)?,
        })
    };
    let rows = match arg {
        Some(a) => stmt
            .query_map(params![a], map)?
            .collect::<rusqlite::Result<Vec<_>>>(),
        None => stmt
            .query_map([], map)?
            .collect::<rusqlite::Result<Vec<_>>>(),
    };
    Ok(rows?)
}

pub fn add_message(
    c: &Connection,
    id: i64,
    author: &str,
    body: &str,
    sent: bool,
) -> anyhow::Result<i64> {
    c.execute(
        "INSERT INTO messages(proposal_id, author, body, draft_version, sent, created_at)
         SELECT ?1, ?2, ?3, version, ?4, ?5 FROM proposals WHERE id = ?1",
        params![id, author, body, sent, fmt_time(now())],
    )?;
    if c.changes() != 1 {
        bail!("card {id} not found");
    }
    Ok(c.last_insert_rowid())
}

fn message_rows(c: &Connection, sql: &str, id: i64) -> anyhow::Result<Vec<MessageRow>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(params![id], |r| {
        Ok(MessageRow {
            id: r.get(0)?,
            author: r.get(1)?,
            body: r.get(2)?,
            draft_version: r.get(3)?,
            sent: r.get(4)?,
            created_at: parse_time(&r.get::<_, String>(5)?)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn messages(c: &Connection, id: i64) -> anyhow::Result<Vec<MessageRow>> {
    message_rows(
        c,
        "SELECT id, author, body, draft_version, sent, created_at FROM messages
         WHERE proposal_id = ?1 ORDER BY id",
        id,
    )
}

/// Marks unsent human messages as sent and returns them.
pub fn mark_sent(c: &Connection, id: i64) -> anyhow::Result<Vec<MessageRow>> {
    let pending = message_rows(
        c,
        "SELECT id, author, body, draft_version, 1, created_at FROM messages
         WHERE proposal_id = ?1 AND sent = 0 AND author = 'human' ORDER BY id",
        id,
    )?;
    c.execute(
        "UPDATE messages SET sent = 1 WHERE proposal_id = ?1 AND sent = 0 AND author = 'human'",
        params![id],
    )?;
    Ok(pending)
}

/// Moves a card along the lifecycle if it is still in `from`.
pub fn advance(c: &Connection, id: i64, from: Status, ev: &Event) -> anyhow::Result<bool> {
    let to = transition(from, ev)?;
    cas_status(c, id, from, to)
}

/// Unsent human messages, oldest first, without changing them.
pub fn pending_human(c: &Connection, id: i64) -> anyhow::Result<Vec<MessageRow>> {
    message_rows(
        c,
        "SELECT id, author, body, draft_version, sent, created_at FROM messages
         WHERE proposal_id = ?1 AND sent = 0 AND author = 'human' ORDER BY id",
        id,
    )
}

pub fn mark_sent_ids(c: &Connection, ids: &[i64]) -> anyhow::Result<()> {
    let mut stmt = c.prepare("UPDATE messages SET sent = 1 WHERE id = ?1")?;
    for id in ids {
        stmt.execute(params![id])?;
    }
    Ok(())
}

/// Cards left in `agent_working` by a previous process; returns them to `ready`.
pub fn reset_interrupted(c: &Connection) -> anyhow::Result<Vec<i64>> {
    let ids: Vec<i64> = {
        let mut stmt = c.prepare("SELECT id FROM proposals WHERE status = 'agent_working'")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for id in &ids {
        cas_status(c, *id, Status::AgentWorking, Status::Ready)?;
        add_message(
            c,
            *id,
            "system",
            "The agent run was interrupted by a service restart. Send again if you still need an answer.",
            true,
        )?;
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(p: &str) -> SourceRow {
        SourceRow {
            permalink: p.into(),
            title: format!("title of {p}"),
            content_hash: format!("hash-{p}"),
            original: format!("raw {p}"),
        }
    }

    fn np(perms: &[&str]) -> NewProposal {
        let action = if perms.len() > 1 {
            Action::Merge
        } else {
            Action::Promote
        };
        NewProposal {
            v: ValidatedProposal {
                action,
                sources: perms.iter().map(|s| s.to_string()).collect(),
                target_dir: Some("ops".into()),
                target_title: Some(format!("T {}", perms[0])),
                draft: Some("- [fact] x".into()),
                tags: vec!["a".into()],
                rationale: "r".into(),
            },
            sources: perms.iter().map(|p| src(p)).collect(),
        }
    }

    async fn insert(db: &Db, perms: &'static [&'static str]) -> i64 {
        db.call(move |c| insert_proposal(c, &np(perms)))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn insert_claims_sources_and_conflict_on_reclaim() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        let claimed = db.call(|c| claimed_permalinks(c)).await.unwrap();
        assert!(claimed.contains("p/inbox/a"));

        let e = db
            .call(|c| insert_proposal(c, &np(&["p/inbox/b", "p/inbox/a"])))
            .await
            .unwrap_err();
        assert!(e.downcast_ref::<ClaimConflict>().is_some(), "{e:#}");
        // The whole insert rolled back: b is not claimed and no second card exists.
        let claimed = db.call(|c| claimed_permalinks(c)).await.unwrap();
        assert!(!claimed.contains("p/inbox/b"));
        let all = db.call(|c| list_queue(c, QueueFilter::All)).await.unwrap();
        assert_eq!(all.len(), 1);

        let p = db
            .call(move |c| get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.status, Status::Ready);
        assert_eq!(p.version, 1);
        assert_eq!(p.tags, vec!["a".to_string()]);
        assert_eq!(p.target_title.as_deref(), Some("T p/inbox/a"));
    }

    #[tokio::test]
    async fn cas_status_only_once() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        assert!(
            db.call(move |c| cas_status(c, id, Status::Ready, Status::Applying))
                .await
                .unwrap()
        );
        assert!(
            !db.call(move |c| cas_status(c, id, Status::Ready, Status::Applying))
                .await
                .unwrap()
        );
        let p = db
            .call(move |c| get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.status, Status::Applying);
    }

    #[tokio::test]
    async fn release_frees_claim() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        db.call(move |c| release_sources(c, id)).await.unwrap();
        assert!(db.call(|c| claimed_permalinks(c)).await.unwrap().is_empty());
        insert(&db, &["p/inbox/a"]).await;
        // Released sources are still listed for the old card (history).
        assert_eq!(db.call(move |c| sources(c, id)).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_draft_bumps_version_and_replaces_sources() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a", "p/inbox/b"]).await;
        let mut n = np(&["p/inbox/a"]);
        n.v.draft = Some("new".into());
        n.sources[0].content_hash = "hash-new".into();
        db.call(move |c| update_draft(c, id, &n.v, &n.sources))
            .await
            .unwrap();

        let p = db
            .call(move |c| get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.version, 2);
        assert_eq!(p.draft.as_deref(), Some("new"));
        assert_eq!(p.action, Action::Promote);
        let s = db.call(move |c| sources(c, id)).await.unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].content_hash, "hash-new");
        // b was dropped from the card and is free again.
        assert!(
            !db.call(|c| claimed_permalinks(c))
                .await
                .unwrap()
                .contains("p/inbox/b")
        );
    }

    #[tokio::test]
    async fn update_draft_conflict_keeps_old_state() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        insert(&db, &["p/inbox/b"]).await;
        let n = np(&["p/inbox/a", "p/inbox/b"]);
        let e = db
            .call(move |c| update_draft(c, id, &n.v, &n.sources))
            .await
            .unwrap_err();
        assert!(e.downcast_ref::<ClaimConflict>().is_some());
        let p = db
            .call(move |c| get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.version, 1);
        assert_eq!(db.call(move |c| sources(c, id)).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unsent_messages_marked_once() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        db.call(move |c| add_message(c, id, "human", "one", false))
            .await
            .unwrap();
        db.call(move |c| add_message(c, id, "human", "two", false))
            .await
            .unwrap();
        db.call(move |c| add_message(c, id, "agent", "reply", true))
            .await
            .unwrap();
        let sent = db.call(move |c| mark_sent(c, id)).await.unwrap();
        assert_eq!(
            sent.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert!(db.call(move |c| mark_sent(c, id)).await.unwrap().is_empty());
        let all = db.call(move |c| messages(c, id)).await.unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|m| m.sent));
        assert_eq!(all[0].draft_version, 1);
    }

    #[tokio::test]
    async fn wake_snoozed_after_deadline() {
        let db = Db::open_in_memory().unwrap();
        let past = insert(&db, &["p/inbox/a"]).await;
        let future = insert(&db, &["p/inbox/b"]).await;
        let t = now();
        assert!(
            db.call(move |c| snooze(c, past, t - time::Duration::seconds(1)))
                .await
                .unwrap()
        );
        assert!(
            db.call(move |c| snooze(c, future, t + time::Duration::days(1)))
                .await
                .unwrap()
        );
        assert_eq!(db.call(move |c| wake_snoozed(c, t)).await.unwrap(), 1);
        let a = db
            .call(move |c| get_proposal(c, past))
            .await
            .unwrap()
            .unwrap();
        let b = db
            .call(move |c| get_proposal(c, future))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.status, Status::Ready);
        assert!(a.snoozed_until.is_none());
        assert_eq!(b.status, Status::Snoozed);
        assert!(b.snoozed_until.is_some());
    }

    #[tokio::test]
    async fn snooze_only_from_ready() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        db.call(move |c| cas_status(c, id, Status::Ready, Status::Applying))
            .await
            .unwrap();
        assert!(!db.call(move |c| snooze(c, id, now())).await.unwrap());
    }

    #[tokio::test]
    async fn queue_excludes_finished_and_orders_unsent_first() {
        let db = Db::open_in_memory().unwrap();
        let a = insert(&db, &["p/inbox/a"]).await;
        let b = insert(&db, &["p/inbox/b"]).await;
        let done = insert(&db, &["p/inbox/c"]).await;
        db.call(move |c| add_message(c, a, "human", "hi", false))
            .await
            .unwrap();
        db.call(move |c| cas_status(c, done, Status::Ready, Status::Applying))
            .await
            .unwrap();
        db.call(move |c| cas_status(c, done, Status::Applying, Status::Accepted))
            .await
            .unwrap();

        let q = db.call(|c| list_queue(c, QueueFilter::Open)).await.unwrap();
        assert_eq!(q.iter().map(|i| i.id).collect::<Vec<_>>(), [a, b]);
        assert_eq!(q[0].unsent, 1);
        assert_eq!(q[0].sources_count, 1);
        assert_eq!(q[0].title, "T p/inbox/a");

        let st = db
            .call(|c| list_queue(c, QueueFilter::Status(Status::Accepted)))
            .await
            .unwrap();
        assert_eq!(st.len(), 1);
    }

    #[tokio::test]
    async fn delete_card_title_comes_from_source() {
        let db = Db::open_in_memory().unwrap();
        let mut n = np(&["p/inbox/a"]);
        n.v.action = Action::Delete;
        n.v.target_title = None;
        n.v.target_dir = None;
        n.v.draft = None;
        db.call(move |c| insert_proposal(c, &n)).await.unwrap();
        let q = db.call(|c| list_queue(c, QueueFilter::Open)).await.unwrap();
        assert_eq!(q[0].title, "title of p/inbox/a");
    }

    #[tokio::test]
    async fn error_and_applied_permalink_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        db.call(move |c| set_error(c, id, Some("boom")))
            .await
            .unwrap();
        db.call(move |c| set_applied_permalink(c, id, "p/verified/ops/t"))
            .await
            .unwrap();
        let p = db
            .call(move |c| get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.error.as_deref(), Some("boom"));
        assert_eq!(p.applied_permalink.as_deref(), Some("p/verified/ops/t"));
        db.call(move |c| set_error(c, id, None)).await.unwrap();
        let p = db
            .call(move |c| get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert!(p.error.is_none());
    }

    #[tokio::test]
    async fn file_db_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/review.db");
        let id = {
            let db = Db::open(&path).unwrap();
            insert(&db, &["p/inbox/a"]).await
        };
        let db = Db::open(&path).unwrap();
        assert!(
            db.call(move |c| get_proposal(c, id))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn advance_follows_domain_table() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        assert!(
            db.call(move |c| advance(c, id, Status::Ready, &Event::Accept))
                .await
                .unwrap()
        );
        // Not in `ready` any more: no-op.
        assert!(
            !db.call(move |c| advance(c, id, Status::Ready, &Event::Accept))
                .await
                .unwrap()
        );
        // Illegal transition is an error, not a silent false.
        assert!(
            db.call(move |c| advance(c, id, Status::Applying, &Event::Snooze))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pending_then_mark_by_ids_keeps_later_comments() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db, &["p/inbox/a"]).await;
        db.call(move |c| add_message(c, id, "human", "first", false))
            .await
            .unwrap();
        let seen = db.call(move |c| pending_human(c, id)).await.unwrap();
        assert_eq!(seen.len(), 1);
        // A comment arrives while the agent works on `seen`.
        db.call(move |c| add_message(c, id, "human", "later", false))
            .await
            .unwrap();
        let ids: Vec<i64> = seen.iter().map(|m| m.id).collect();
        db.call(move |c| mark_sent_ids(c, &ids)).await.unwrap();
        let left = db.call(move |c| pending_human(c, id)).await.unwrap();
        assert_eq!(
            left.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            ["later"]
        );
    }

    #[tokio::test]
    async fn reset_interrupted_returns_cards_to_ready_with_note() {
        let db = Db::open_in_memory().unwrap();
        let a = insert(&db, &["p/inbox/a"]).await;
        let b = insert(&db, &["p/inbox/b"]).await;
        db.call(move |c| cas_status(c, a, Status::Ready, Status::AgentWorking))
            .await
            .unwrap();
        let ids = db.call(|c| reset_interrupted(c)).await.unwrap();
        assert_eq!(ids, [a]);
        let p = db.call(move |c| get_proposal(c, a)).await.unwrap().unwrap();
        assert_eq!(p.status, Status::Ready);
        let m = db.call(move |c| messages(c, a)).await.unwrap();
        assert_eq!(m.last().unwrap().author, "system");
        assert!(db.call(move |c| messages(c, b)).await.unwrap().is_empty());
    }
}
