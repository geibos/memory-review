//! Runtime configuration, read from `MR_*` environment variables.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};

/// Interface language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    pub mcp_url: String,
    pub project: String,
    pub llm_url: String,
    pub llm_key: String,
    pub model: String,
    /// Scheme + host (+ port) the browser uses, without a trailing slash.
    pub public_origin: String,
    pub lang: Lang,
    pub inbox_dir: String,
    pub verified_dir: String,
    pub snooze_days: i64,
    pub prompts_dir: Option<PathBuf>,
}

const REQUIRED: [&str; 6] = [
    "MR_MCP_URL",
    "MR_PROJECT",
    "MR_LLM_URL",
    "MR_LLM_KEY",
    "MR_MODEL",
    "MR_PUBLIC_ORIGIN",
];

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        Config::from_map(&std::env::vars().collect())
    }

    pub fn from_map(m: &HashMap<String, String>) -> anyhow::Result<Config> {
        let get = |k: &str| m.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());

        let missing: Vec<&str> = REQUIRED.into_iter().filter(|k| get(k).is_none()).collect();
        if !missing.is_empty() {
            bail!("missing required env: {}", missing.join(", "));
        }
        let req = |k: &str| get(k).unwrap_or_default().to_string();

        let lang = match get("MR_LANG").unwrap_or("en") {
            "en" => Lang::En,
            "ru" => Lang::Ru,
            other => bail!("MR_LANG must be `en` or `ru`, got `{other}`"),
        };
        let bind = get("MR_BIND")
            .unwrap_or("0.0.0.0:8080")
            .parse()
            .context("MR_BIND must be host:port")?;
        let snooze_days = get("MR_SNOOZE_DAYS")
            .unwrap_or("7")
            .parse()
            .context("MR_SNOOZE_DAYS must be an integer")?;

        Ok(Config {
            bind,
            db_path: PathBuf::from(get("MR_DB_PATH").unwrap_or("data/review.db")),
            mcp_url: req("MR_MCP_URL"),
            project: req("MR_PROJECT"),
            llm_url: req("MR_LLM_URL").trim_end_matches('/').to_string(),
            llm_key: req("MR_LLM_KEY"),
            model: req("MR_MODEL"),
            public_origin: req("MR_PUBLIC_ORIGIN").trim_end_matches('/').to_string(),
            lang,
            inbox_dir: get("MR_INBOX_DIR").unwrap_or("inbox").to_string(),
            verified_dir: get("MR_VERIFIED_DIR").unwrap_or("verified").to_string(),
            snooze_days,
            prompts_dir: get("MR_PROMPTS_DIR").map(PathBuf::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_map() -> HashMap<String, String> {
        [
            ("MR_MCP_URL", "http://memory.example.org/mcp"),
            ("MR_PROJECT", "notes"),
            ("MR_LLM_URL", "http://llm.example.org"),
            ("MR_LLM_KEY", "k"),
            ("MR_MODEL", "some-model"),
            ("MR_PUBLIC_ORIGIN", "https://review.example.org"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn missing_required_lists_all_names() {
        let err = Config::from_map(&HashMap::new()).unwrap_err().to_string();
        for k in REQUIRED {
            assert!(err.contains(k), "{k} missing from: {err}");
        }
    }

    #[test]
    fn defaults_applied() {
        let c = Config::from_map(&required_map()).unwrap();
        assert_eq!(c.inbox_dir, "inbox");
        assert_eq!(c.verified_dir, "verified");
        assert_eq!(c.snooze_days, 7);
        assert_eq!(c.lang, Lang::En);
        assert_eq!(c.bind.port(), 8080);
        assert_eq!(c.db_path, PathBuf::from("data/review.db"));
        assert!(c.prompts_dir.is_none());
    }

    #[test]
    fn origin_trailing_slash_trimmed() {
        let mut m = required_map();
        m.insert(
            "MR_PUBLIC_ORIGIN".into(),
            "https://review.example.org/".into(),
        );
        assert_eq!(
            Config::from_map(&m).unwrap().public_origin,
            "https://review.example.org"
        );
    }

    #[test]
    fn bad_lang_rejected() {
        let mut m = required_map();
        m.insert("MR_LANG".into(), "de".into());
        assert!(Config::from_map(&m).is_err());
    }

    #[test]
    fn ru_lang_and_overrides() {
        let mut m = required_map();
        m.insert("MR_LANG".into(), "ru".into());
        m.insert("MR_SNOOZE_DAYS".into(), "3".into());
        m.insert("MR_INBOX_DIR".into(), "incoming".into());
        let c = Config::from_map(&m).unwrap();
        assert_eq!(c.lang, Lang::Ru);
        assert_eq!(c.snooze_days, 3);
        assert_eq!(c.inbox_dir, "incoming");
    }

    #[test]
    fn blank_required_counts_as_missing() {
        let mut m = required_map();
        m.insert("MR_MODEL".into(), "  ".into());
        let err = Config::from_map(&m).unwrap_err().to_string();
        assert!(err.contains("MR_MODEL"));
    }
}
