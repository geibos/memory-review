//! Accepting a card: the only place that writes to the memory vault.

use crate::db::{self, Db};
use crate::domain::{Action, Event, Status};
use crate::memory::{MemoryApi, WriteOutcome};
use crate::note::{content_hash, same_body};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyResult {
    Accepted,
    /// A source changed or vanished; nothing was written.
    Stale,
    /// The card was not in a state this action applies to.
    AlreadyHandled,
    /// Stopped part-way; the card stays in `applying` and can be retried.
    Failed(String),
}

/// Accepts a `ready` card.
///
/// cancel-safe: NO — once the card is `applying`, cancelling between the write
/// and the deletes leaves it there; `retry` finishes the job idempotently.
pub async fn accept(
    db: &Db,
    memory: &dyn MemoryApi,
    verified_dir: &str,
    id: i64,
) -> anyhow::Result<ApplyResult> {
    let card = load(db, id).await?;
    if card.status != Status::Ready {
        return Ok(ApplyResult::AlreadyHandled);
    }
    let rows = db.call(move |c| db::sources(c, id)).await?;
    let mut missing = 0;
    let mut changed = false;
    for r in &rows {
        match memory.read_exact(&r.permalink).await? {
            None => missing += 1,
            Some(n) if content_hash(&n.raw) != r.content_hash => changed = true,
            Some(_) => {}
        }
    }
    if missing == rows.len() {
        db.call(move |c| {
            if db::advance(c, id, Status::Ready, &Event::SourcesGone)? {
                db::release_sources(c, id)?;
            }
            Ok(())
        })
        .await?;
        return Ok(ApplyResult::Stale);
    }
    if missing > 0 || changed {
        db.call(move |c| db::advance(c, id, Status::Ready, &Event::SourcesChanged))
            .await?;
        return Ok(ApplyResult::Stale);
    }
    if !db
        .call(move |c| db::advance(c, id, Status::Ready, &Event::Accept))
        .await?
    {
        return Ok(ApplyResult::AlreadyHandled);
    }
    run(db, memory, verified_dir, id).await
}

/// Re-runs a card stuck in `applying`.
///
/// cancel-safe: NO — same as [`accept`]; running it again is always safe.
pub async fn retry(
    db: &Db,
    memory: &dyn MemoryApi,
    verified_dir: &str,
    id: i64,
) -> anyhow::Result<ApplyResult> {
    if load(db, id).await?.status != Status::Applying {
        return Ok(ApplyResult::AlreadyHandled);
    }
    run(db, memory, verified_dir, id).await
}

async fn load(db: &Db, id: i64) -> anyhow::Result<db::ProposalRow> {
    db.call(move |c| db::get_proposal(c, id))
        .await?
        .ok_or_else(|| anyhow::anyhow!("card {id} not found"))
}

/// Writes the verified note (if any), then deletes the sources. Every step
/// checks what is already done, so it can run any number of times.
async fn run(
    db: &Db,
    memory: &dyn MemoryApi,
    verified_dir: &str,
    id: i64,
) -> anyhow::Result<ApplyResult> {
    let card = load(db, id).await?;
    let rows = db.call(move |c| db::sources(c, id)).await?;
    let steps = async {
        if card.action != Action::Delete {
            ensure_written(db, memory, verified_dir, &card).await?;
        }
        for r in &rows {
            let Some(current) = memory.read_exact(&r.permalink).await? else {
                continue; // already deleted
            };
            if content_hash(&current.raw) != r.content_hash {
                anyhow::bail!(
                    "{} was edited after the card was accepted; it was not deleted",
                    r.permalink
                );
            }
            memory.delete(&r.permalink).await?;
            if memory.read_exact(&r.permalink).await?.is_some() {
                anyhow::bail!("deleting {} did not take effect", r.permalink);
            }
        }
        anyhow::Ok(())
    }
    .await;
    match steps {
        Ok(()) => {
            db.call(move |c| {
                db::release_sources(c, id)?;
                db::set_error(c, id, None)?;
                db::advance(c, id, Status::Applying, &Event::ApplyDone)
            })
            .await?;
            Ok(ApplyResult::Accepted)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let stored = msg.clone();
            db.call(move |c| db::set_error(c, id, Some(&stored)))
                .await?;
            Ok(ApplyResult::Failed(msg))
        }
    }
}

async fn ensure_written(
    db: &Db,
    memory: &dyn MemoryApi,
    verified_dir: &str,
    card: &db::ProposalRow,
) -> anyhow::Result<()> {
    let (Some(dir), Some(title), Some(draft)) = (&card.target_dir, &card.target_title, &card.draft)
    else {
        anyhow::bail!("card {} has no target or draft", card.id);
    };
    if let Some(p) = &card.applied_permalink
        && let Some(n) = memory.read_exact(p).await?
        && same_body(&n.raw, draft)
    {
        return Ok(());
    }
    let folder = format!("{verified_dir}/{dir}");
    let permalink = match memory.write(&folder, title, draft, &card.tags).await? {
        WriteOutcome::Created { permalink } => permalink,
        WriteOutcome::Conflict => {
            let existing = memory
                .list_dir(&folder)
                .await?
                .into_iter()
                .find(|e| &e.title == title);
            let same = match &existing {
                Some(e) => memory
                    .read_exact(&e.permalink)
                    .await?
                    .is_some_and(|n| same_body(&n.raw, draft)),
                None => false,
            };
            match existing {
                Some(e) if same => e.permalink,
                _ => anyhow::bail!(
                    "name conflict: a different note titled “{title}” already exists in {folder}; rename the target in a comment"
                ),
            }
        }
    };
    let id = card.id;
    db.call(move |c| db::set_applied_permalink(c, id, &permalink))
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::db::{NewProposal, SourceRow};
    use crate::domain::ValidatedProposal;
    use crate::memory::fake::{FakeMemory, raw_note};

    async fn card(db: &Db, mem: &FakeMemory, action: Action, sources: &[&str]) -> i64 {
        let rows: Vec<SourceRow> = sources
            .iter()
            .map(|p| {
                let raw = mem.raw(p).unwrap();
                SourceRow {
                    permalink: p.to_string(),
                    title: "t".into(),
                    content_hash: content_hash(&raw),
                    original: raw,
                }
            })
            .collect();
        let writes = action != Action::Delete;
        let v = ValidatedProposal {
            action,
            sources: sources.iter().map(|s| s.to_string()).collect(),
            target_dir: writes.then(|| "ops".into()),
            target_title: writes.then(|| "Final".into()),
            draft: writes.then(|| "- [fact] final".into()),
            tags: vec!["x".into()],
            rationale: "r".into(),
        };
        db.call(move |c| db::insert_proposal(c, &NewProposal { v, sources: rows }))
            .await
            .unwrap()
    }

    async fn status(db: &Db, id: i64) -> Status {
        db.call(move |c| db::get_proposal(c, id))
            .await
            .unwrap()
            .unwrap()
            .status
    }

    fn setup() -> (Db, FakeMemory) {
        (Db::open_in_memory().unwrap(), FakeMemory::default())
    }

    #[tokio::test]
    async fn accept_promote_writes_and_deletes() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "- [fact] a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Accepted
        );
        assert!(mem.raw(&a).is_none());
        let written = mem.raw("p/verified/ops/final").unwrap();
        assert!(written.ends_with("- [fact] final"));
        assert_eq!(status(&db, id).await, Status::Accepted);
        assert!(
            db.call(|c| db::claimed_permalinks(c))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn accept_merge_deletes_all_sources() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let b = mem.add("inbox", "B", "b");
        let id = card(&db, &mem, Action::Merge, &[&a, &b]).await;
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Accepted
        );
        assert!(mem.raw(&a).is_none() && mem.raw(&b).is_none());
        assert_eq!(mem.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn accept_delete_only_deletes() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Delete, &[&a]).await;
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Accepted
        );
        assert!(mem.raw(&a).is_none());
        assert_eq!(mem.writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn accept_with_changed_source_marks_stale() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        mem.set_raw(&a, raw_note(&a, "A", "edited in Obsidian"));
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Stale
        );
        assert_eq!(status(&db, id).await, Status::Stale);
        assert_eq!(mem.writes.load(Ordering::SeqCst), 0);
        assert!(mem.raw(&a).is_some());
    }

    #[tokio::test]
    async fn accept_with_missing_source_marks_stale() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let b = mem.add("inbox", "B", "b");
        let id = card(&db, &mem, Action::Merge, &[&a, &b]).await;
        mem.remove(&b);
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Stale
        );
        assert_eq!(status(&db, id).await, Status::Stale);
        assert!(mem.raw(&a).is_some());
        assert_eq!(
            mem.writes.load(Ordering::SeqCst) + mem.deletes.load(Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn accept_with_all_sources_missing_closes() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        mem.remove(&a);
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Stale
        );
        assert_eq!(status(&db, id).await, Status::Closed);
        assert!(
            db.call(|c| db::claimed_permalinks(c))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn double_accept_writes_once() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        let (r1, r2) = tokio::join!(
            accept(&db, &mem, "verified", id),
            accept(&db, &mem, "verified", id)
        );
        let mut results = [r1.unwrap(), r2.unwrap()];
        results.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(
            results,
            [ApplyResult::Accepted, ApplyResult::AlreadyHandled]
        );
        assert_eq!(mem.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn accept_on_non_ready_is_already_handled() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        db.call(move |c| db::cas_status(c, id, Status::Ready, Status::Snoozed))
            .await
            .unwrap();
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::AlreadyHandled
        );
    }

    #[tokio::test]
    async fn write_conflict_same_body_is_success() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        mem.add("verified/ops", "Final", "- [fact] final\n");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        assert_eq!(
            accept(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Accepted
        );
        assert!(mem.raw(&a).is_none());
        let p = db
            .call(move |c| db::get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.applied_permalink.as_deref(), Some("p/verified/ops/final"));
    }

    #[tokio::test]
    async fn write_conflict_different_body_is_error() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        mem.add("verified/ops", "Final", "something else entirely");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        let r = accept(&db, &mem, "verified", id).await.unwrap();
        assert!(
            matches!(&r, ApplyResult::Failed(e) if e.contains("Final")),
            "{r:?}"
        );
        assert_eq!(status(&db, id).await, Status::Applying);
        assert!(
            mem.raw(&a).is_some(),
            "source kept when the write did not land"
        );
        assert!(
            mem.raw("p/verified/ops/final")
                .unwrap()
                .contains("something else")
        );
        let p = db
            .call(move |c| db::get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert!(p.error.is_some());
    }

    #[tokio::test]
    async fn failure_after_write_then_retry_completes() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        mem.fail_next.lock().unwrap().push("delete");
        let r = accept(&db, &mem, "verified", id).await.unwrap();
        assert!(matches!(r, ApplyResult::Failed(_)), "{r:?}");
        assert!(mem.raw("p/verified/ops/final").is_some());
        assert!(mem.raw(&a).is_some());
        assert_eq!(status(&db, id).await, Status::Applying);

        assert_eq!(
            retry(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Accepted
        );
        assert_eq!(
            mem.writes.load(Ordering::SeqCst),
            1,
            "no second write on retry"
        );
        assert!(mem.raw(&a).is_none());
        let p = db
            .call(move |c| db::get_proposal(c, id))
            .await
            .unwrap()
            .unwrap();
        assert!(p.error.is_none());
    }

    #[tokio::test]
    async fn retry_after_lost_write_response_detects_existing_note() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        // The write "failed" from our side, yet the note landed (lost response).
        mem.fail_next.lock().unwrap().push("write");
        mem.add("verified/ops", "Final", "- [fact] final");
        let r = accept(&db, &mem, "verified", id).await.unwrap();
        assert!(matches!(r, ApplyResult::Failed(_)));
        assert_eq!(
            retry(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::Accepted
        );
    }

    #[tokio::test]
    async fn delete_that_did_not_take_effect_is_error() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Delete, &[&a]).await;
        mem.delete_noop.store(true, Ordering::SeqCst);
        let r = accept(&db, &mem, "verified", id).await.unwrap();
        assert!(matches!(r, ApplyResult::Failed(_)), "{r:?}");
        assert_eq!(status(&db, id).await, Status::Applying);
    }

    #[tokio::test]
    async fn retry_refuses_to_delete_source_edited_meanwhile() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        mem.fail_next.lock().unwrap().push("delete");
        accept(&db, &mem, "verified", id).await.unwrap();
        mem.set_raw(&a, raw_note(&a, "A", "new knowledge added after accepting"));
        let r = retry(&db, &mem, "verified", id).await.unwrap();
        assert!(matches!(r, ApplyResult::Failed(_)), "{r:?}");
        assert!(mem.raw(&a).unwrap().contains("new knowledge"));
    }

    #[tokio::test]
    async fn retry_only_from_applying() {
        let (db, mem) = setup();
        let a = mem.add("inbox", "A", "a");
        let id = card(&db, &mem, Action::Promote, &[&a]).await;
        assert_eq!(
            retry(&db, &mem, "verified", id).await.unwrap(),
            ApplyResult::AlreadyHandled
        );
        assert_eq!(mem.writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_card_is_error() {
        let (db, mem) = setup();
        assert!(accept(&db, &mem, "verified", 42).await.is_err());
    }
}
