//! `janus identity ...` (`docs/admin-surface#identities-endpoint`): the
//! opaque-identity registry. `create` prints the new token exactly once, on
//! stdout; nothing else the CLI prints ever contains a token.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use clap::Subcommand;
use http::Method;
use janus_core::contract::registry::IdentityRecord;
use serde::Deserialize;
use serde_json::{json, Value};

use super::Ctx;
use crate::client::{api_path, decode};
use crate::error::CliError;
use crate::render::{self, fields, Table};

#[derive(Subcommand)]
pub enum IdentityCommand {
    /// Register an identity and print its token, once. In human output the
    /// token is the only thing on stdout (metadata goes to stderr), so
    /// `TOKEN=$(janus identity create ...)` captures just the token.
    Create {
        /// The caller_identity policy will see.
        #[arg(long, value_name = "STRING")]
        identity: String,
        /// A claim, becoming identity_claims; repeatable.
        #[arg(long = "claim", value_name = "K=V", value_parser = parse_claim)]
        claims: Vec<(String, String)>,
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// Expire the entry after this long (e.g. 90s, 12h, 30d).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration, conflicts_with = "expires_at")]
        expires_in: Option<String>,
        /// Expire the entry at this RFC 3339 time.
        #[arg(long, value_name = "TIME")]
        expires_at: Option<DateTime<Utc>>,
    },
    /// List registry entries (active ones unless told otherwise).
    List {
        #[arg(long, value_name = "STRING")]
        identity: Option<String>,
        #[arg(long)]
        include_revoked: bool,
        #[arg(long)]
        include_expired: bool,
    },
    /// One entry.
    Show { id: String },
    /// Revoke an entry: the next request presenting its token is rejected.
    Revoke { id: String },
    /// Remove an entry outright.
    #[command(visible_alias = "delete")]
    Rm { id: String },
}

fn parse_claim(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err(format!("expected K=V, got {text:?}")),
    }
}

fn parse_duration(text: &str) -> Result<String, String> {
    humantime::parse_duration(text)
        .map(|_| text.to_owned())
        .map_err(|e| format!("{text:?} is not a duration such as 90s, 12h or 30d: {e}"))
}

#[derive(Deserialize)]
struct IdentityList {
    identities: Vec<IdentityRecord>,
}

pub async fn run(ctx: &Ctx, command: &IdentityCommand) -> Result<(), CliError> {
    match command {
        IdentityCommand::Create {
            identity,
            claims,
            label,
            expires_in,
            expires_at,
        } => {
            let mut map = BTreeMap::new();
            for (k, v) in claims {
                if map.insert(k.clone(), v.clone()).is_some() {
                    return Err(CliError::usage(format!("--claim {k} is given twice")));
                }
            }
            let mut body = json!({ "identity": identity, "claims": map });
            if let Some(l) = label {
                body["label"] = l.clone().into();
            }
            if let Some(d) = expires_in {
                body["expires_in"] = d.clone().into();
            }
            if let Some(at) = expires_at {
                body["expires_at"] = json!(at);
            }
            let path = api_path(&["v1", "identities"], &[])?;
            let raw = ctx
                .admin()?
                .call(Method::POST, &path, Some(&body))
                .await?
                .ok_or_else(|| CliError::failed("bad_response", "the create answer had no body"))?;
            let token = raw
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| CliError::failed("bad_response", "the create answer has no token"))?
                .to_owned();
            if ctx.json {
                return render::json(&raw);
            }
            let record: IdentityRecord = decode(&path, &raw)?;
            render::note(&format!(
                "created identity {} for {:?} (expires {}); the token below is shown once and cannot be retrieved again",
                record.id,
                record.identity,
                render::time(record.expires_at)
            ));
            render::out(&format!("{token}\n"))
        }
        IdentityCommand::List {
            identity,
            include_revoked,
            include_expired,
        } => {
            let flag = |b: bool| b.then(|| "true".to_owned());
            let path = api_path(
                &["v1", "identities"],
                &[
                    ("identity", identity.clone()),
                    ("include_revoked", flag(*include_revoked)),
                    ("include_expired", flag(*include_expired)),
                ],
            )?;
            let (raw, list): (Value, IdentityList) =
                ctx.admin()?.json(Method::GET, &path, None).await?;
            if ctx.json {
                return render::json(&raw);
            }
            render::out(&table(&list.identities))
        }
        IdentityCommand::Show { id } | IdentityCommand::Revoke { id } => {
            let (method, path) = match command {
                IdentityCommand::Revoke { .. } => (
                    Method::POST,
                    api_path(&["v1", "identities", id, "revoke"], &[])?,
                ),
                _ => (Method::GET, api_path(&["v1", "identities", id], &[])?),
            };
            let (raw, record): (Value, IdentityRecord) =
                ctx.admin()?.json(method, &path, None).await?;
            if ctx.json {
                return render::json(&raw);
            }
            let mut text = match command {
                IdentityCommand::Revoke { .. } => format!("revoked identity {}\n", record.id),
                _ => String::new(),
            };
            text.push_str(&detail(&record));
            render::out(&text)
        }
        IdentityCommand::Rm { id } => {
            ctx.admin()?
                .call(
                    Method::DELETE,
                    &api_path(&["v1", "identities", id], &[])?,
                    None,
                )
                .await?;
            if ctx.json {
                render::json(&json!({ "deleted": id }))
            } else {
                render::out(&format!("removed identity {id}\n"))
            }
        }
    }
}

fn claims_text(claims: &BTreeMap<String, String>) -> String {
    if claims.is_empty() {
        return "-".to_owned();
    }
    claims
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn table(identities: &[IdentityRecord]) -> String {
    if identities.is_empty() {
        return "no identities\n".to_owned();
    }
    let mut t = Table::new(&[
        "ID",
        "IDENTITY",
        "CREATED",
        "EXPIRES",
        "REVOKED",
        "LAST SEEN",
        "LABEL",
        "CLAIMS",
    ]);
    for r in identities {
        t.row(vec![
            r.id.clone(),
            r.identity.clone(),
            render::time(Some(r.created_at)),
            render::time(r.expires_at),
            render::time(r.revoked_at),
            render::time(r.last_seen_at),
            render::text(r.label.as_deref()),
            claims_text(&r.claims),
        ]);
    }
    t.render()
}

fn detail(r: &IdentityRecord) -> String {
    fields(&[
        ("id", r.id.clone()),
        ("identity", r.identity.clone()),
        ("claims", claims_text(&r.claims)),
        ("label", render::text(r.label.as_deref())),
        ("created", render::time(Some(r.created_at))),
        ("expires", render::time(r.expires_at)),
        ("revoked", render::time(r.revoked_at)),
        ("last seen", render::time(r.last_seen_at)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_and_durations_parse_strictly() {
        assert_eq!(
            parse_claim("team=infra=core").unwrap(),
            ("team".into(), "infra=core".into())
        );
        assert_eq!(
            parse_claim("empty=").unwrap(),
            ("empty".into(), String::new())
        );
        assert!(parse_claim("=v").is_err());
        assert!(parse_claim("novalue").is_err());
        assert_eq!(parse_duration("30d").unwrap(), "30d");
        assert!(parse_duration("soon").is_err());
    }
}
