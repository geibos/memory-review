//! The memory store as this service sees it: list, read, search, write, delete.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::mcp::McpClient;
use crate::note::{RawNote, frontmatter_field};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxEntry {
    pub permalink: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub permalink: String,
    pub title: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Created {
        permalink: String,
    },
    /// A note with this title already exists in that folder.
    Conflict,
}

// async-trait (dyn): the service holds `Arc<dyn MemoryApi>` so tests can swap in a fake.
#[async_trait]
pub trait MemoryApi: Send + Sync {
    /// Notes directly inside `dir` (all pages).
    async fn list_dir(&self, dir: &str) -> anyhow::Result<Vec<InboxEntry>>;
    /// Names of sub-folders directly inside `dir`.
    async fn list_dirs(&self, dir: &str) -> anyhow::Result<Vec<String>>;
    /// The note with exactly this permalink, or `None`.
    async fn read_exact(&self, permalink: &str) -> anyhow::Result<Option<RawNote>>;
    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>>;
    /// Creates a note; never overwrites.
    async fn write(
        &self,
        dir: &str,
        title: &str,
        body: &str,
        tags: &[String],
    ) -> anyhow::Result<WriteOutcome>;
    /// Deletes a note by permalink; `Ok(false)` if there was nothing to delete.
    async fn delete(&self, permalink: &str) -> anyhow::Result<bool>;
}

/// Permalinks of the notes physically inside `inbox_dir`.
///
/// A permalink alone does not say where a note lives: Basic Memory keeps the
/// old permalink when a file is moved by hand (`update_permalinks_on_move` is
/// off by default), so `inbox/...` may point at a note already in `verified/`.
///
/// cancel-safe: yes — read-only.
pub async fn inbox_permalinks(
    memory: &dyn MemoryApi,
    inbox_dir: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    Ok(memory
        .list_dir(inbox_dir)
        .await?
        .into_iter()
        .map(|e| e.permalink)
        .collect())
}

/// [`MemoryApi`] backed by a Basic Memory MCP server.
pub struct McpMemory {
    client: McpClient,
    project: String,
}

impl McpMemory {
    pub fn new(url: String, project: String) -> McpMemory {
        McpMemory {
            client: McpClient::new(url),
            project,
        }
    }

    /// Calls a tool with `project` and JSON output, returning the parsed payload.
    async fn tool_json(&self, name: &str, mut args: Value) -> anyhow::Result<Value> {
        args["project"] = json!(self.project);
        args["output_format"] = json!("json");
        let r = self.client.call_tool(name, args).await?;
        if r.is_error {
            anyhow::bail!(
                "{name} failed: {}",
                r.text.chars().take(300).collect::<String>()
            );
        }
        serde_json::from_str(&r.text).map_err(|e| {
            anyhow::anyhow!(
                "{name} returned non-JSON text ({e}): {}",
                r.text.chars().take(200).collect::<String>()
            )
        })
    }

    /// All nodes of a directory listing, across pages.
    async fn nodes(&self, dir: &str) -> anyhow::Result<Vec<Value>> {
        let mut out = Vec::new();
        for page in 1..=50 {
            let v = self
                .tool_json(
                    "list_directory",
                    json!({ "dir_name": format!("/{}", dir.trim_matches('/')), "depth": 1,
                            "page": page, "page_size": 200 }),
                )
                .await?;
            if let Some(nodes) = v["nodes"].as_array() {
                out.extend(nodes.iter().cloned());
            }
            if !v["has_more"].as_bool().unwrap_or(false) {
                return Ok(out);
            }
        }
        anyhow::bail!("directory {dir} has more than 10000 entries")
    }
}

#[async_trait]
impl MemoryApi for McpMemory {
    async fn list_dir(&self, dir: &str) -> anyhow::Result<Vec<InboxEntry>> {
        Ok(self
            .nodes(dir)
            .await?
            .iter()
            .filter(|n| n["type"] == "file")
            .filter_map(|n| {
                Some(InboxEntry {
                    permalink: n["permalink"].as_str()?.to_string(),
                    title: n["title"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    async fn list_dirs(&self, dir: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .nodes(dir)
            .await?
            .iter()
            .filter(|n| n["type"] == "directory")
            .filter_map(|n| n["name"].as_str().map(str::to_string))
            .collect())
    }

    async fn read_exact(&self, permalink: &str) -> anyhow::Result<Option<RawNote>> {
        // read_content does not take output_format; its text is already JSON.
        let r = self
            .client
            .call_tool(
                "read_content",
                json!({ "project": self.project, "path": permalink }),
            )
            .await?;
        if r.is_error {
            // Basic Memory reports a missing note either like this or with a
            // fuzzy match (handled below); any other error is a real failure.
            if r.text.starts_with("Resource not found") {
                return Ok(None);
            }
            anyhow::bail!(
                "read_content failed: {}",
                r.text.chars().take(300).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&r.text)?;
        let Some(raw) = v["text"].as_str() else {
            return Ok(None);
        };
        // Basic Memory answers an unknown path with the closest other note.
        if frontmatter_field(raw, "permalink").as_deref() != Some(permalink) {
            return Ok(None);
        }
        Ok(Some(RawNote {
            permalink: permalink.to_string(),
            title: frontmatter_field(raw, "title").unwrap_or_default(),
            raw: raw.to_string(),
        }))
    }

    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        let v = self
            .tool_json(
                "search_notes",
                json!({ "query": query, "page_size": limit }),
            )
            .await?;
        let mut hits: Vec<SearchHit> = Vec::new();
        for r in v["results"].as_array().into_iter().flatten() {
            let Some(permalink) = r["permalink"].as_str() else {
                continue;
            };
            if r["type"] != "entity" || hits.iter().any(|h| h.permalink == permalink) {
                continue;
            }
            hits.push(SearchHit {
                permalink: permalink.to_string(),
                title: r["title"].as_str().unwrap_or_default().to_string(),
                snippet: r["content"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect(),
            });
        }
        Ok(hits)
    }

    async fn write(
        &self,
        dir: &str,
        title: &str,
        body: &str,
        tags: &[String],
    ) -> anyhow::Result<WriteOutcome> {
        let v = self
            .tool_json(
                "write_note",
                json!({ "directory": dir, "title": title, "content": body,
                        "tags": tags.join(","), "overwrite": false }),
            )
            .await?;
        match v["action"].as_str() {
            Some("created") => Ok(WriteOutcome::Created {
                permalink: v["permalink"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("write_note returned no permalink"))?
                    .to_string(),
            }),
            Some("conflict") => Ok(WriteOutcome::Conflict),
            other => anyhow::bail!("write_note returned unexpected action {other:?}"),
        }
    }

    async fn delete(&self, permalink: &str) -> anyhow::Result<bool> {
        let v = self
            .tool_json("delete_note", json!({ "identifier": permalink }))
            .await?;
        Ok(v["deleted"].as_bool().unwrap_or(false))
    }
}

#[cfg(test)]
pub mod fake {
    //! In-memory [`MemoryApi`] for tests. Permalinks look like `p/<dir>/<slug>`.

    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    pub struct FakeMemory {
        pub notes: Mutex<BTreeMap<String, String>>,
        /// Operation names (`"write"`, `"delete"`, …) whose next call fails.
        pub fail_next: Mutex<Vec<&'static str>>,
        /// When set, `delete` reports success but keeps the note.
        pub delete_noop: AtomicBool,
        pub writes: AtomicUsize,
        pub deletes: AtomicUsize,
        /// Notes moved by hand: permalink → folder the file now lives in.
        /// Basic Memory keeps the old permalink on a move by default.
        pub moved: Mutex<BTreeMap<String, String>>,
    }

    pub fn slug(title: &str) -> String {
        title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect()
    }

    pub fn raw_note(permalink: &str, title: &str, body: &str) -> String {
        format!("---\ntitle: {title}\ntype: note\npermalink: {permalink}\n---\n\n{body}")
    }

    impl FakeMemory {
        /// Adds a note at `p/<dir>/<slug(title)>` and returns its permalink.
        pub fn add(&self, dir: &str, title: &str, body: &str) -> String {
            let permalink = format!("p/{dir}/{}", slug(title));
            self.notes
                .lock()
                .unwrap()
                .insert(permalink.clone(), raw_note(&permalink, title, body));
            permalink
        }

        pub fn raw(&self, permalink: &str) -> Option<String> {
            self.notes.lock().unwrap().get(permalink).cloned()
        }

        pub fn set_raw(&self, permalink: &str, raw: String) {
            self.notes
                .lock()
                .unwrap()
                .insert(permalink.to_string(), raw);
        }

        /// Moves the file to `dir` without changing its permalink.
        pub fn move_to(&self, permalink: &str, dir: &str) {
            self.moved
                .lock()
                .unwrap()
                .insert(permalink.to_string(), dir.to_string());
        }

        fn folder(&self, permalink: &str) -> String {
            if let Some(d) = self.moved.lock().unwrap().get(permalink) {
                return d.clone();
            }
            let rest = permalink.strip_prefix("p/").unwrap_or(permalink);
            rest.rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default()
        }

        pub fn remove(&self, permalink: &str) {
            self.notes.lock().unwrap().remove(permalink);
        }

        fn maybe_fail(&self, op: &'static str) -> anyhow::Result<()> {
            let mut f = self.fail_next.lock().unwrap();
            if let Some(i) = f.iter().position(|x| *x == op) {
                f.remove(i);
                anyhow::bail!("injected failure in {op}");
            }
            Ok(())
        }

        fn entry(permalink: &str, raw: &str) -> InboxEntry {
            InboxEntry {
                permalink: permalink.to_string(),
                title: frontmatter_field(raw, "title").unwrap_or_default(),
            }
        }
    }

    #[async_trait]
    impl MemoryApi for FakeMemory {
        async fn list_dir(&self, dir: &str) -> anyhow::Result<Vec<InboxEntry>> {
            self.maybe_fail("list_dir")?;
            let notes = self.notes.lock().unwrap().clone();
            Ok(notes
                .iter()
                .filter(|(p, _)| self.folder(p) == dir)
                .map(|(p, raw)| Self::entry(p, raw))
                .collect())
        }

        async fn list_dirs(&self, dir: &str) -> anyhow::Result<Vec<String>> {
            let prefix = format!("p/{dir}/");
            let mut dirs: Vec<String> = self
                .notes
                .lock()
                .unwrap()
                .keys()
                .filter_map(|p| {
                    p.strip_prefix(&prefix)?
                        .split_once('/')
                        .map(|(d, _)| d.to_string())
                })
                .collect();
            dirs.dedup();
            Ok(dirs)
        }

        async fn read_exact(&self, permalink: &str) -> anyhow::Result<Option<RawNote>> {
            self.maybe_fail("read")?;
            Ok(self.raw(permalink).map(|raw| RawNote {
                permalink: permalink.to_string(),
                title: frontmatter_field(&raw, "title").unwrap_or_default(),
                raw,
            }))
        }

        async fn search(&self, _query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
            self.maybe_fail("search")?;
            Ok(self
                .notes
                .lock()
                .unwrap()
                .iter()
                .take(limit)
                .map(|(p, raw)| SearchHit {
                    permalink: p.clone(),
                    title: frontmatter_field(raw, "title").unwrap_or_default(),
                    snippet: crate::note::body(raw).chars().take(100).collect(),
                })
                .collect())
        }

        async fn write(
            &self,
            dir: &str,
            title: &str,
            body: &str,
            _tags: &[String],
        ) -> anyhow::Result<WriteOutcome> {
            self.maybe_fail("write")?;
            self.writes.fetch_add(1, Ordering::SeqCst);
            let permalink = format!("p/{dir}/{}", slug(title));
            let mut notes = self.notes.lock().unwrap();
            if notes.contains_key(&permalink) {
                return Ok(WriteOutcome::Conflict);
            }
            notes.insert(permalink.clone(), raw_note(&permalink, title, body));
            Ok(WriteOutcome::Created { permalink })
        }

        async fn delete(&self, permalink: &str) -> anyhow::Result<bool> {
            self.maybe_fail("delete")?;
            self.deletes.fetch_add(1, Ordering::SeqCst);
            if self.delete_noop.load(Ordering::SeqCst) {
                return Ok(true);
            }
            Ok(self.notes.lock().unwrap().remove(permalink).is_some())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::test_server;

    fn read_content(raw: &str) -> Value {
        json!({"type": "text", "text": raw})
    }

    #[tokio::test]
    async fn read_exact_returns_matching_note() {
        let (url, stub) = test_server::start(|_, args| {
            let p = args["path"].as_str().unwrap();
            read_content(&format!("---\ntitle: 'A: b'\npermalink: {p}\n---\n\nbody"))
        })
        .await;
        let m = McpMemory::new(url, "proj".into());
        let n = m.read_exact("proj/inbox/a").await.unwrap().unwrap();
        assert_eq!(n.permalink, "proj/inbox/a");
        assert_eq!(n.title, "A: b");
        assert!(n.raw.ends_with("body"));
        assert_eq!(stub.calls.lock().unwrap()[0].1["project"], "proj");
    }

    #[tokio::test]
    async fn read_exact_rejects_substituted_note() {
        // Basic Memory answers an unknown permalink with its best fuzzy match.
        let (url, _stub) = test_server::start(|_, _| {
            read_content("---\ntitle: Other\npermalink: proj/inbox/other\n---\n\nx")
        })
        .await;
        let m = McpMemory::new(url, "proj".into());
        assert!(m.read_exact("proj/inbox/missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_exact_not_found_error_is_none() {
        // After a delete Basic Memory answers with an error instead of a fuzzy match.
        let (url, _stub) = test_server::start(|_, args| {
            json!({"__is_error": true,
                   "__raw_text": format!("Resource not found: {}", args["path"].as_str().unwrap())})
        })
        .await;
        let m = McpMemory::new(url, "proj".into());
        assert!(m.read_exact("proj/inbox/gone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_exact_tool_error_is_error() {
        let (url, _stub) =
            test_server::start(|_, _| json!({"__is_error": true, "__raw_text": "db locked"})).await;
        let m = McpMemory::new(url, "proj".into());
        assert!(m.read_exact("proj/inbox/a").await.is_err());
    }

    #[tokio::test]
    async fn list_dir_pages_through_has_more() {
        let (url, stub) = test_server::start(|_, args| {
            let page = args["page"].as_i64().unwrap();
            let file = |n: i64| json!({"type":"file","title":format!("N{n}"),"permalink":format!("proj/inbox/n{n}")});
            let dir = json!({"type":"directory","title":null,"permalink":null});
            if page == 1 {
                json!({"nodes":[file(1), dir, file(2)],"page":1,"has_more":true})
            } else {
                json!({"nodes":[file(3)],"page":2,"has_more":false})
            }
        })
        .await;
        let m = McpMemory::new(url, "proj".into());
        let notes = m.list_dir("inbox").await.unwrap();
        assert_eq!(
            notes.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(),
            ["N1", "N2", "N3"]
        );
        let calls = stub.calls.lock().unwrap();
        assert_eq!(calls[0].1["dir_name"], "/inbox");
        assert_eq!(calls.len(), 2);
    }

    #[tokio::test]
    async fn list_dirs_returns_directory_names() {
        let (url, _stub) = test_server::start(|_, _| {
            json!({"nodes":[{"type":"directory","name":"infra","directory_path":"/verified/infra"},
                            {"type":"file","title":"x","permalink":"proj/verified/x"}],"has_more":false})
        })
        .await;
        let m = McpMemory::new(url, "proj".into());
        assert_eq!(m.list_dirs("verified").await.unwrap(), ["infra"]);
    }

    #[tokio::test]
    async fn write_created_and_conflict() {
        let (url, stub) = test_server::start(|_, args| {
            if args["title"] == "Taken" {
                json!({"title":"Taken","permalink":"verified/taken","file_path":null,"action":"conflict","error":"NOTE_ALREADY_EXISTS"})
            } else {
                json!({"title":"New","permalink":"proj/verified/ops/new","file_path":"verified/ops/New.md","action":"created"})
            }
        })
        .await;
        let m = McpMemory::new(url, "proj".into());
        let tags = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            m.write("verified/ops", "New", "body", &tags).await.unwrap(),
            WriteOutcome::Created {
                permalink: "proj/verified/ops/new".into()
            }
        );
        assert_eq!(
            m.write("verified/ops", "Taken", "body", &[]).await.unwrap(),
            WriteOutcome::Conflict
        );
        let calls = stub.calls.lock().unwrap();
        assert_eq!(calls[0].1["overwrite"], false);
        assert_eq!(calls[0].1["directory"], "verified/ops");
        assert_eq!(calls[0].1["tags"], "a,b");
    }

    #[tokio::test]
    async fn write_unknown_action_is_error() {
        let (url, _stub) = test_server::start(|_, _| json!({"action":"updated"})).await;
        let m = McpMemory::new(url, "proj".into());
        assert!(m.write("verified", "X", "b", &[]).await.is_err());
    }

    #[tokio::test]
    async fn delete_reports_flag() {
        let (url, _stub) =
            test_server::start(|_, args| json!({"deleted": args["identifier"] == "proj/inbox/a"}))
                .await;
        let m = McpMemory::new(url, "proj".into());
        assert!(m.delete("proj/inbox/a").await.unwrap());
        assert!(!m.delete("proj/inbox/b").await.unwrap());
    }

    #[tokio::test]
    async fn search_keeps_entities_only_and_dedupes() {
        let (url, _stub) = test_server::start(|_, _| {
            json!({"results":[
                {"type":"entity","title":"A","permalink":"proj/inbox/a","content":"aaa"},
                {"type":"observation","title":"A","permalink":"proj/inbox/a/obs","content":"o"},
                {"type":"entity","title":"A","permalink":"proj/inbox/a","content":"dup"},
                {"type":"entity","title":"B","permalink":"proj/verified/b","content":"bbb"}]})
        })
        .await;
        let m = McpMemory::new(url, "proj".into());
        let hits = m.search("q", 8).await.unwrap();
        assert_eq!(
            hits.iter()
                .map(|h| h.permalink.as_str())
                .collect::<Vec<_>>(),
            ["proj/inbox/a", "proj/verified/b"]
        );
        assert_eq!(hits[0].snippet, "aaa");
    }
}
