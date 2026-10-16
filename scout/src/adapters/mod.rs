//! Website-specific extraction strategies.
//!
//! A strategy carries an explicit `status` so the API can report what has
//! actually been verified, rather than implying uniform support.

pub mod youtube;

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyStatus {
    /// Exercised end to end against fixtures *and* a live site.
    VerifiedLive,
    /// Exercised against fixtures only.
    VerifiedFixtures,
    /// Implemented but only ever run against a synthetic fixture.
    FixtureVerifiedOnly,
    /// Deliberately not implemented; the API reports why.
    NotImplemented,
}

impl StrategyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StrategyStatus::VerifiedLive => "verified-live",
            StrategyStatus::VerifiedFixtures => "verified-fixtures",
            StrategyStatus::FixtureVerifiedOnly => "fixture-verified-only",
            StrategyStatus::NotImplemented => "not_implemented",
        }
    }
    /// Whether the strategy is safe to run in the default (network) mode.
    pub fn is_runnable(self) -> bool {
        matches!(
            self,
            StrategyStatus::VerifiedLive | StrategyStatus::VerifiedFixtures | StrategyStatus::FixtureVerifiedOnly
        )
    }
}

pub struct StrategyInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub status: StrategyStatus,
    pub reason: Option<&'static str>,
    pub default_limit: usize,
}

pub fn registry() -> Vec<StrategyInfo> {
    vec![
        StrategyInfo {
            name: "generic-page",
            description: "Title, description, canonical URL, Open Graph, headings, links, <time> elements and JSON-LD.",
            status: StrategyStatus::VerifiedFixtures,
            reason: None,
            default_limit: 50,
        },
        StrategyInfo {
            name: "selectors",
            description: "Extract fields using CSS selectors from a caller-supplied schema.",
            status: StrategyStatus::VerifiedFixtures,
            reason: None,
            default_limit: 50,
        },
        StrategyInfo {
            name: "youtube-channel-videos",
            description: "Channel identity and recent videos (title, id, URL, duration, view count, published text) from a channel Videos page.",
            status: StrategyStatus::VerifiedLive,
            reason: None,
            default_limit: 30,
        },
        StrategyInfo {
            name: "facebook-jobs",
            description: "Public job posts.",
            status: StrategyStatus::NotImplemented,
            reason: Some(
                "Facebook job pages require a full browser and authentication Scout does not have; implementing this would mean either fabricating results or bypassing access controls",
            ),
            default_limit: 20,
        },
        StrategyInfo {
            name: "facebook-marketplace",
            description: "Accessible marketplace listings.",
            status: StrategyStatus::NotImplemented,
            reason: Some(
                "Facebook Marketplace is authenticated and JavaScript-rendered; Scout does not bypass authentication",
            ),
            default_limit: 20,
        },
        StrategyInfo {
            name: "tiktok-profile",
            description: "Profile identity and recent posts.",
            status: StrategyStatus::NotImplemented,
            reason: Some(
                "TikTok serves profile content through JavaScript and active anti-bot controls; Scout does not evade bot protections",
            ),
            default_limit: 20,
        },
    ]
}

pub fn list_json() -> Value {
    let items: Vec<Value> = registry()
        .into_iter()
        .map(|s| {
            let mut o = json!({
                "name": s.name,
                "description": s.description,
                "status": s.status.as_str(),
            });
            if let Some(r) = s.reason {
                o["reason"] = json!(r);
            }
            o
        })
        .collect();
    json!({ "strategies": items })
}
