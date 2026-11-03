//! What a custom build passes to the daemon's entry point alongside its
//! config (`docs/plugin-contract#custom-build`).
//!
//! The stock binary passes `Embedded::default()`: no plugin can be linked
//! and no custom extractor registered. A custom build passes its compiled-in
//! `JanusPlugin` and/or `IdentityExtractor`s. Config and linkage have to
//! agree; [`Embedded::check_against`] is that startup check.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use janus_core::contract::binding::BindingSpec;
use janus_core::contract::identity::IdentityExtractor;
use janus_core::contract::plugin::JanusPlugin;

use super::config::{invalid, ConfigError, DaemonConfig};

/// Implementations compiled into this build of the daemon.
#[derive(Clone, Default)]
pub struct Embedded {
    /// A linked plugin; requires `[plugin] linked = true`.
    pub plugin: Option<Arc<dyn JanusPlugin>>,
    /// Custom extractors, referenced from config as `method = "custom:<name>"`
    /// by their `IdentityExtractor::name`.
    pub extractors: Vec<Arc<dyn IdentityExtractor>>,
}

impl fmt::Debug for Embedded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.extractors.iter().map(|e| e.name()).collect();
        f.debug_struct("Embedded")
            .field("plugin", &self.plugin.as_ref().map(|_| "linked"))
            .field("extractors", &names)
            .finish()
    }
}

impl Embedded {
    /// The registered custom extractor named `name`, if any.
    pub fn extractor(&self, name: &str) -> Option<&Arc<dyn IdentityExtractor>> {
        self.extractors.iter().find(|e| e.name() == name)
    }

    /// Refuse to start when config and linkage disagree:
    ///
    /// - config selects the linked model but no plugin is linked;
    /// - a plugin is linked but config does not say `linked = true` (an
    ///   endpoint, or no `[plugin]` block at all);
    /// - a binding names `custom:<name>` and no extractor of that name is
    ///   registered, or two registered extractors share a name.
    pub fn check_against(&self, config: &DaemonConfig) -> Result<(), ConfigError> {
        match (&config.plugin, &self.plugin) {
            (Some(p), None) if p.linked => {
                return Err(invalid(
                    "plugin.linked",
                    "config selects a linked plugin but this build links none; refusing to start as an open pass-through proxy",
                ))
            }
            (Some(p), Some(_)) if p.endpoint.is_some() => {
                return Err(invalid(
                    "plugin.endpoint",
                    "config names a plugin endpoint and this build also links a plugin; ambiguous",
                ))
            }
            (None, Some(_)) => {
                return Err(invalid(
                    "plugin",
                    "this build links a plugin but config has no [plugin] block with linked = true",
                ))
            }
            _ => {}
        }

        let mut names = BTreeSet::new();
        for e in &self.extractors {
            if !names.insert(e.name()) {
                return Err(invalid(
                    "extractors",
                    format!("two custom extractors are registered as {:?}", e.name()),
                ));
            }
        }
        for up in config.resolve_bindings()? {
            for (at, spec) in up.located() {
                if let BindingSpec::Custom(c) = spec {
                    if !names.contains(c.name.as_str()) {
                        return Err(invalid(
                            format!("{at}.method"),
                            format!(
                                "custom extractor {:?} is not registered in this build",
                                c.name
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use janus_core::contract::identity::{
        ExtractedIdentity, ExtractionError, PresentedAt, RequestHead,
    };
    use janus_core::contract::plugin::{NotifyEvent, PluginError, ResolveRoute, RouteOutcome};

    struct Allow;

    #[async_trait]
    impl JanusPlugin for Allow {
        async fn resolve_route(&self, _req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
            Ok(RouteOutcome::UseDefault)
        }

        async fn notify(&self, _event: NotifyEvent) {}
    }

    struct Introspect;

    #[async_trait]
    impl IdentityExtractor for Introspect {
        fn name(&self) -> &str {
            "introspect"
        }

        async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
            let value = req
                .headers
                .get("x-token")
                .and_then(|v| v.to_str().ok())
                .ok_or(ExtractionError::NoMaterial)?;
            Ok(ExtractedIdentity {
                caller_identity: value.to_owned(),
                claims: Default::default(),
                correlation_id: None,
                presented_at: PresentedAt::Header(http::HeaderName::from_static("x-token")),
            })
        }
    }

    fn config(extra: &str) -> DaemonConfig {
        DaemonConfig::from_toml_str(&format!("[data_plane]\nbind = \"127.0.0.1:0\"\n{extra}"))
            .expect("config is valid on its own")
    }

    fn linked() -> Embedded {
        Embedded {
            plugin: Some(Arc::new(Allow)),
            extractors: vec![],
        }
    }

    #[test]
    fn stock_build_is_empty_and_agrees_with_plugin_less_config() {
        let stock = Embedded::default();
        assert!(stock.plugin.is_none() && stock.extractors.is_empty());
        stock.check_against(&config("")).expect("pass-through");
        stock
            .check_against(&config("[plugin]\nendpoint = \"unix:///run/p.sock\"\n"))
            .expect("grpc plugin with nothing linked");
        linked()
            .check_against(&config("[plugin]\nlinked = true\n"))
            .expect("linked plugin, linked config");
    }

    #[test]
    fn config_and_linkage_disagreement_refuses_to_start() {
        let err = Embedded::default()
            .check_against(&config("[plugin]\nlinked = true\n"))
            .expect_err("linked config, nothing linked");
        assert_eq!(err.at(), Some("plugin.linked"));
        let err = linked()
            .check_against(&config("[plugin]\nendpoint = \"unix:///run/p.sock\"\n"))
            .expect_err("endpoint and linked plugin");
        assert_eq!(err.at(), Some("plugin.endpoint"));
        let err = linked()
            .check_against(&config(""))
            .expect_err("linked plugin, no [plugin] block");
        assert_eq!(err.at(), Some("plugin"));
    }

    #[test]
    fn custom_extractor_references_must_be_registered() {
        let cfg = config(
            "[[upstream]]\nname = \"anthropic\"\npath_prefix = \"/anthropic\"\nbase_url = \"https://api.anthropic.com\"\n[upstream.identity]\nmethod = \"custom:introspect\"\n",
        );
        let err = Embedded::default()
            .check_against(&cfg)
            .expect_err("unregistered custom extractor");
        assert_eq!(err.at(), Some("upstream[anthropic].identity.method"));
        let with = Embedded {
            plugin: None,
            extractors: vec![Arc::new(Introspect)],
        };
        with.check_against(&cfg).expect("registered");
        assert!(with.extractor("introspect").is_some());
        assert!(format!("{with:?}").contains("introspect"));
        let twice = Embedded {
            plugin: None,
            extractors: vec![Arc::new(Introspect), Arc::new(Introspect)],
        };
        assert_eq!(
            twice.check_against(&cfg).expect_err("duplicate").at(),
            Some("extractors")
        );
    }

    #[test]
    fn custom_extractor_references_are_checked_at_route_level_too() {
        let cfg = config(
            "[[upstream]]\nname = \"openai\"\npath_prefix = \"/openai\"\nbase_url = \"https://api.openai.com\"\n[upstream.identity]\nmethod = \"none\"\n[[upstream.route]]\npath = \"/v1/chat/completions\"\nidentity = { method = \"custom:introspect\" }\n",
        );
        let err = Embedded::default()
            .check_against(&cfg)
            .expect_err("unregistered route-level custom extractor");
        assert_eq!(
            err.at(),
            Some("upstream[openai].route[/v1/chat/completions].identity.method")
        );
        // Registered under another name is still unregistered.
        struct Other;
        #[async_trait]
        impl IdentityExtractor for Other {
            fn name(&self) -> &str {
                "other"
            }
            async fn extract(
                &self,
                _req: &RequestHead,
            ) -> Result<ExtractedIdentity, ExtractionError> {
                Err(ExtractionError::NoMaterial)
            }
        }
        let other = Embedded {
            plugin: None,
            extractors: vec![Arc::new(Other)],
        };
        assert!(other.check_against(&cfg).is_err());
        assert!(other.extractor("introspect").is_none());
        Embedded {
            plugin: None,
            extractors: vec![Arc::new(Introspect)],
        }
        .check_against(&cfg)
        .expect("registered");
    }
}
