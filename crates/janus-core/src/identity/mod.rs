//! Caller identity extraction: the `hmac`, `jwt`, `opaque` and
//! `trusted_header` verification methods, construction of extractors from a
//! resolved [`crate::contract::binding::BindingSpec`], and the log-only
//! unverified decode. Codes against [`crate::contract::identity`]. See
//! `docs/caller-identity`.
//!
//! - [`IdentityFactory`] is the
//!   [`ExtractorFactory`](crate::contract::identity::ExtractorFactory) for
//!   the four shipped methods.
//! - [`BindingTable`] is every binding of a config, constructed at startup
//!   (route > upstream > daemon default, with the default already merged into
//!   the upstream level by the config layer). [`BindingTable::builder`]
//!   resolves each binding's named material through a [`MaterialResolver`],
//!   constructs its extractor, and refuses to finish on a missing secret, an
//!   unusable key, an unreachable JWKS endpoint, a missing registry or an
//!   unregistered custom extractor.
//! - [`Binding::identify`] runs one request head through the selected
//!   binding: a verified identity, `None` on a `method = "none"` route, or a
//!   [`Rejection`] that carries the log-only claimed identity.
//!
//! Decisions the docs leave open, made here:
//!
//! - Material location. A header or query source that appears more than once
//!   is `invalid` (ambiguous). A header present without the binding's scheme
//!   prefix (matched case-insensitively, as HTTP auth schemes are) is
//!   `invalid`; an absent or empty value is `no_material`.
//! - Check order. Signature first, so nothing about an unauthenticated token
//!   is parsed or reported; then namespace (`hmac`) or issuer and audience
//!   (`jwt`), reported as `wrong_namespace`; then lifetime. A token that is
//!   both expired and for another binding is reported as the wrong binding.
//! - Lifetime. `exp` is required by both signed methods and a token is
//!   expired from the second `exp` names onward, the same boundary as the
//!   opaque registry's `expires_at`. A `jwt` `nbf` in the future (beyond
//!   [`JWT_NBF_SKEW`]) is `invalid`.
//! - Claims. `claims` holds every verified claim except the identity claim
//!   and the method's own machinery (`ns`, `exp` for `hmac`; `iss`, `aud`,
//!   `exp`, `nbf`, `iat` for `jwt`). Strings are kept as-is, numbers and
//!   booleans as their JSON text, arrays and objects as compact JSON; `null`
//!   is dropped. The identity claim must be a non-empty string or a number.
//! - Correlation. A `correlation_claim` the verified material does not carry
//!   yields no correlation value rather than a rejection: it is a grouping
//!   hint. A custom extractor's own `correlation_id` is kept unless its
//!   binding names a correlation source, which then wins.
//! - A verified identity is never empty; an empty `caller_identity` is
//!   reserved for `method = "none"` routes.

mod binding;
mod claims;
mod custom;
mod factory;
mod hmac;
mod jwt;
mod opaque;
mod source;
#[cfg(test)]
mod testkit;
mod trusted_header;

pub use binding::{
    Binding, BindingTable, BindingTableBuilder, MaterialResolver, Rejection, Selected,
    StartupError, Unbound,
};
pub use factory::{Clock, IdentityFactory, JwksRefresh, JWT_NBF_SKEW};
