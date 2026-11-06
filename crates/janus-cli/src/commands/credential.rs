//! `janus credential ...` (`docs/admin-surface#credentials-endpoint`).
//!
//! Material enters only from standard input (`--api-key-stdin`) or a file
//! (`--from-file`), never from an argument, and is never printed: every
//! response the daemon sends back is metadata.

use std::io::{BufRead, IsTerminal, Read};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use http::Method;
use janus_core::contract::credential::CredentialMetadata;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{upstream_parser, Ctx};
use crate::client::api_path;
use crate::error::CliError;
use crate::render::{self, fields, Table};

#[derive(Subcommand)]
pub enum CredentialCommand {
    /// List the pool's credentials (metadata only).
    List {
        /// Only the credentials this upstream instance may use; an
        /// implementation name lists all of that implementation's.
        #[arg(long, value_name = "NAME")]
        upstream: Option<String>,
    },
    /// One credential's metadata.
    Show { r#ref: String },
    /// The credential the default chain would pick for an upstream
    /// instance right now. The pick has no side effects.
    Default {
        /// An upstream instance name, or an implementation name.
        #[arg(long, value_name = "NAME")]
        upstream: String,
        /// The verified caller identity a `sticky` instance would key on.
        #[arg(long, value_name = "IDENTITY")]
        identity: Option<String>,
    },
    /// Add a credential to the pool; live without a restart.
    Add {
        #[arg(long, value_name = "NAME", value_parser = upstream_parser())]
        upstream: String,
        /// The pool slot's name (generated when absent).
        #[arg(long = "ref", value_name = "REF")]
        credential_ref: Option<String>,
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// A tag an `[upstream.credentials] tags` filter can select on;
        /// repeat for several.
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        #[command(flatten)]
        material: MaterialArgs,
    },
    /// Replace a pool slot's tags; no `--tag` clears them.
    Tag {
        r#ref: String,
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
    },
    /// Replace a credential's material and clear an `invalid` status.
    Rotate {
        r#ref: String,
        #[command(flatten)]
        material: MaterialArgs,
    },
    /// Put a credential back into the default chain and plugin resolution.
    Enable { r#ref: String },
    /// Take a credential out of the default chain and plugin resolution.
    Disable { r#ref: String },
    /// Refresh an expiring OAuth credential now rather than on next use.
    Refresh { r#ref: String },
    /// Remove the pool slot.
    #[command(visible_alias = "delete")]
    Rm { r#ref: String },
}

/// Where material comes from. There is deliberately no flag that takes a
/// key as an argument.
#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct MaterialArgs {
    /// Read a bare API key from standard input (hidden when typed at a terminal).
    #[arg(long)]
    api_key_stdin: bool,
    /// Read the material from a file: a vendor CLI's credential file (JSON),
    /// or a file holding a bare API key.
    #[arg(long, value_name = "PATH")]
    from_file: Option<PathBuf>,
}

impl MaterialArgs {
    fn read(&self) -> Result<Value, CliError> {
        match &self.from_file {
            Some(path) => material_from_file(path),
            None => Ok(json!({ "api_key": api_key_from_stdin()? })),
        }
    }
}

/// A credential file's contents as the `material` document: a JSON file is
/// sent as its document, anything else as the bare key it holds.
fn material_from_file(path: &Path) -> Result<Value, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::usage(format!("cannot read --from-file {}: {e}", path.display())))?;
    parse_material(&text)
        .map_err(|why| CliError::usage(format!("--from-file {}: {why}", path.display())))
}

fn parse_material(text: &str) -> Result<Value, &'static str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("the file is empty");
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(doc @ (Value::Object(_) | Value::String(_))) => Ok(doc),
        Ok(_) => Err("a JSON credential file must hold an object"),
        // Not JSON: the file is a bare key. The daemon refuses text that
        // only looks like a broken JSON document rather than send it as a
        // key.
        Err(_) => Ok(Value::String(trimmed.to_owned())),
    }
}

fn api_key_from_stdin() -> Result<String, CliError> {
    let stdin = std::io::stdin();
    let mut text = String::new();
    let read = if stdin.is_terminal() {
        eprint!("API key (input hidden): ");
        let echo = EchoOff::new();
        let read = stdin.lock().read_line(&mut text);
        drop(echo);
        eprintln!();
        read
    } else {
        stdin.lock().read_to_string(&mut text)
    };
    read.map_err(|e| CliError::usage(format!("cannot read the API key from standard input: {e}")))?;
    let key = text.trim();
    if key.is_empty() {
        return Err(CliError::usage(
            "--api-key-stdin: no API key on standard input",
        ));
    }
    Ok(key.to_owned())
}

/// Terminal echo off for as long as this lives, so a typed key is not shown.
struct EchoOff(Option<libc::termios>);

impl EchoOff {
    fn new() -> Self {
        // SAFETY: `termios` is plain data; tcgetattr fills it or fails, and
        // only a filled one is used.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return EchoOff(None);
            }
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) != 0 {
                return EchoOff(None);
            }
            EchoOff(Some(saved))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            // SAFETY: restores the attributes tcgetattr returned.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}

#[derive(Deserialize)]
struct CredentialList {
    credentials: Vec<CredentialMetadata>,
}

pub async fn run(ctx: &Ctx, command: &CredentialCommand) -> Result<(), CliError> {
    let one = |r: &str, action: Option<&str>| {
        let mut segments = vec!["v1", "credentials", r];
        segments.extend(action);
        api_path(&segments, &[])
    };
    // Material is read before connecting, so a bad source fails without a
    // round trip.
    let request = match command {
        CredentialCommand::List { upstream } => {
            let path = api_path(&["v1", "credentials"], &[("upstream", upstream.clone())])?;
            let admin = ctx.admin()?;
            let (raw, list): (Value, CredentialList) = admin.json(Method::GET, &path, None).await?;
            if ctx.json {
                return render::json(&raw);
            }
            return render::out(&table(&list.credentials));
        }
        CredentialCommand::Show { r#ref } => (Method::GET, one(r#ref, None)?, None),
        CredentialCommand::Default { upstream, identity } => (
            Method::GET,
            api_path(
                &["v1", "credentials", "default"],
                &[
                    ("upstream", Some(upstream.clone())),
                    ("identity", identity.clone()),
                ],
            )?,
            None,
        ),
        CredentialCommand::Add {
            upstream,
            credential_ref,
            label,
            tags,
            material,
        } => {
            let mut body = json!({ "upstream": upstream, "material": material.read()? });
            if let Some(r) = credential_ref {
                body["ref"] = r.clone().into();
            }
            if let Some(l) = label {
                body["label"] = l.clone().into();
            }
            if !tags.is_empty() {
                body["tags"] = json!(tags);
            }
            (
                Method::POST,
                api_path(&["v1", "credentials"], &[])?,
                Some(body),
            )
        }
        CredentialCommand::Rotate { r#ref, material } => (
            Method::PUT,
            one(r#ref, Some("material"))?,
            Some(json!({ "material": material.read()? })),
        ),
        CredentialCommand::Tag { r#ref, tags } => (
            Method::PUT,
            one(r#ref, Some("tags"))?,
            Some(json!({ "tags": tags })),
        ),
        CredentialCommand::Enable { r#ref } => (Method::POST, one(r#ref, Some("enable"))?, None),
        CredentialCommand::Disable { r#ref } => (Method::POST, one(r#ref, Some("disable"))?, None),
        CredentialCommand::Refresh { r#ref } => (Method::POST, one(r#ref, Some("refresh"))?, None),
        CredentialCommand::Rm { r#ref } => {
            ctx.admin()?
                .call(Method::DELETE, &one(r#ref, None)?, None)
                .await?;
            return if ctx.json {
                render::json(&json!({ "deleted": r#ref }))
            } else {
                render::out(&format!("removed credential {}\n", r#ref))
            };
        }
    };
    let (method, path, body) = request;
    let (raw, meta): (Value, CredentialMetadata) =
        ctx.admin()?.json(method, &path, body.as_ref()).await?;
    if ctx.json {
        return render::json(&raw);
    }
    let verb = match command {
        CredentialCommand::Add { .. } => Some("added"),
        CredentialCommand::Rotate { .. } => Some("rotated"),
        CredentialCommand::Enable { .. } => Some("enabled"),
        CredentialCommand::Disable { .. } => Some("disabled"),
        CredentialCommand::Refresh { .. } => Some("refreshed"),
        CredentialCommand::Tag { .. } => Some("tagged"),
        _ => None,
    };
    let mut text = verb.map_or_else(String::new, |v| {
        format!("{v} credential {}\n", meta.credential_ref)
    });
    text.push_str(&detail(&meta));
    render::out(&text)
}

fn table(credentials: &[CredentialMetadata]) -> String {
    if credentials.is_empty() {
        return "no credentials\n".to_owned();
    }
    let mut t = Table::new(&[
        "REF",
        "UPSTREAM",
        "KIND",
        "SOURCE",
        "STATUS",
        "REQUESTS",
        "ERRORS",
        "LAST USED",
        "LABEL",
        "TAGS",
    ]);
    for c in credentials {
        t.row(vec![
            c.credential_ref.clone(),
            c.upstream.to_string(),
            render::name(&c.kind),
            render::name(&c.source),
            render::name(&c.status),
            c.request_count.to_string(),
            c.error_count.to_string(),
            render::time(c.last_used_at),
            render::text(c.label.as_deref()),
            tags_text(&c.tags),
        ]);
    }
    t.render()
}

fn detail(c: &CredentialMetadata) -> String {
    fields(&[
        ("ref", c.credential_ref.clone()),
        ("upstream", c.upstream.to_string()),
        ("kind", render::name(&c.kind)),
        ("label", render::text(c.label.as_deref())),
        ("tags", tags_text(&c.tags)),
        ("source", render::name(&c.source)),
        ("status", render::name(&c.status)),
        ("rate limited until", render::time(c.rate_limited_until)),
        ("expires", render::time(c.expires_at)),
        ("last used", render::time(c.last_used_at)),
        ("requests", c.request_count.to_string()),
        ("errors", c.error_count.to_string()),
        ("last error", render::text(c.last_error.as_deref())),
    ])
}

fn tags_text(tags: &[String]) -> String {
    if tags.is_empty() {
        "-".to_owned()
    } else {
        tags.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_material_is_its_json_document_or_the_bare_key() {
        assert_eq!(
            parse_material("{\"claudeAiOauth\":{\"accessToken\":\"a\"}}\n").unwrap(),
            json!({"claudeAiOauth": {"accessToken": "a"}})
        );
        assert_eq!(
            parse_material("  sk-or-v1-abc\n").unwrap(),
            Value::String("sk-or-v1-abc".into())
        );
        assert!(parse_material("\n").is_err());
        assert!(parse_material("[1,2]").is_err());
        assert_eq!(
            parse_material("{\"tokens\":{\"acc").unwrap(),
            Value::String("{\"tokens\":{\"acc".into()),
            "a broken document goes as text, which the daemon refuses as a key"
        );
    }
}
