//! XISF decoding.
//!
//! Element layout, attribute lists, the scalar grammar of §8.3 and the block location syntax
//! of §10.3 are in the XISF 1.0 specification. What lives here is what it leaves open, what
//! this crate pins because a wrong guess is silent, and where it knowingly diverges.

pub(crate) mod attributes;
pub(crate) mod block;
pub(crate) mod cache;
pub(crate) mod codec;
pub(crate) mod decoder;
pub(crate) mod image;
pub(crate) mod keywords;
pub(crate) mod scalars;
pub(crate) mod xml;

pub(crate) use decoder::Decoder;
