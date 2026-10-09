//! netweir's fetching engine.
//!
//! A [`Fetcher`] sends requests that are byte-for-byte what its
//! [`Profile`]'s browser sends: the same TLS ClientHello, HTTP/2 settings
//! and header order.

mod decode;
mod fetch;
mod profile;

pub use decode::decode;
pub use fetch::{FetchError, FetchErrorKind, FetchOptions, Fetcher, Response};
pub use profile::{DEFAULT as DEFAULT_PROFILE, Profile, ProfileError};
