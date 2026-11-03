//! [`IdentityFactory`]: the [`ExtractorFactory`] that constructs the four
//! shipped extractors from an [`ExtractorInput`].

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::contract::identity::{
    ExtractorBuildError, ExtractorFactory, ExtractorInput, IdentityExtractor,
};

use super::hmac::HmacExtractor;
use super::jwt::JwtExtractor;
use super::opaque::OpaqueExtractor;
use super::trusted_header::TrustedHeaderExtractor;

/// How far in the future a `jwt` `nbf` may be and still be accepted: clock
/// skew between an external issuer and this host. `exp` gets no allowance.
pub const JWT_NBF_SKEW: Duration = Duration::from_secs(60);

/// The time source lifetimes are checked against.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>);

impl Clock {
    /// Wall-clock time.
    pub fn system() -> Self {
        Clock(Arc::new(Utc::now))
    }

    /// A clock reading `now` from `f` (tests, simulations).
    pub fn from_fn(f: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        Clock(Arc::new(f))
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.0)()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Clock").field(&self.now()).finish()
    }
}

/// How a JWKS-backed `jwt` extractor keeps its key set current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JwksRefresh {
    /// Keys older than this are refetched before the next verification.
    pub max_age: Duration,
    /// At most one refetch per this interval, however many requests present
    /// an unknown `kid`: a caller cannot turn Janus into a JWKS flooder.
    pub min_interval: Duration,
    /// Deadline on one fetch, connect included.
    pub timeout: Duration,
}

impl Default for JwksRefresh {
    fn default() -> Self {
        JwksRefresh {
            max_age: Duration::from_secs(600),
            min_interval: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Constructs the `hmac`, `jwt`, `opaque` and `trusted_header` extractors.
#[derive(Debug, Clone)]
pub struct IdentityFactory {
    clock: Clock,
    jwks_refresh: JwksRefresh,
}

impl Default for IdentityFactory {
    fn default() -> Self {
        IdentityFactory::new()
    }
}

impl IdentityFactory {
    /// System clock, default JWKS refresh policy.
    pub fn new() -> Self {
        IdentityFactory {
            clock: Clock::system(),
            jwks_refresh: JwksRefresh::default(),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_jwks_refresh(mut self, refresh: JwksRefresh) -> Self {
        self.jwks_refresh = refresh;
        self
    }
}

#[async_trait]
impl ExtractorFactory for IdentityFactory {
    async fn build(
        &self,
        input: ExtractorInput,
    ) -> Result<Arc<dyn IdentityExtractor>, ExtractorBuildError> {
        Ok(match input {
            ExtractorInput::Hmac { spec, secret } => {
                Arc::new(HmacExtractor::new(spec, &secret, self.clock.clone())?)
            }
            ExtractorInput::Jwt { spec, key } => {
                Arc::new(JwtExtractor::new(spec, key, self.clock.clone(), self.jwks_refresh).await?)
            }
            ExtractorInput::Opaque { spec, registry } => {
                Arc::new(OpaqueExtractor::new(spec, registry, self.clock.clone()))
            }
            ExtractorInput::TrustedHeader(spec) => Arc::new(TrustedHeaderExtractor::new(spec)),
        })
    }
}
