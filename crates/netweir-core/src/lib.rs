//! netweir's fetching engine.
//!
//! A [`Fetcher`] sends requests that are byte-for-byte what its
//! [`Profile`]'s browser sends: the same TLS ClientHello, HTTP/2 settings
//! and header order.

pub mod canonical;
mod crawl;
mod decode;
mod fetch;
mod profile;
pub mod robots;
pub mod tdmrep;

pub use crawl::{CrawlRequest, CrawlSettings, Crawler, DropReason, Event, Stats, Submitted};
pub use decode::{decode, meta_content};
pub use fetch::{FetchError, FetchErrorKind, FetchOptions, Fetcher, Hop, Response};
pub use profile::{DEFAULT as DEFAULT_PROFILE, Profile, ProfileError};
