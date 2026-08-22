//! FITS decoding.
//!
//! Structure, card layout, the data-unit size formula and the value grammar are in FITS
//! Standard 4.0. What lives here is what the standard leaves open.

pub(crate) mod cards;
pub(crate) mod decoder;

pub(crate) use decoder::Decoder;
