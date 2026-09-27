#![forbid(unsafe_code)]
// Annex B names paired quantities `x0`/`y0`, `tbx0`/`tby0`, `ppx`/`ppy`, and
// this decoder uses those names so that the geometry can be checked against the
// standard line by line. Renaming one of each pair would make that harder, not
// safer.
#![allow(clippy::similar_names)]
// A codec is fixed-width arithmetic. Every cast below is a deliberate change of
// width -- a byte pulled out of a register, a coefficient handed to a float
// filter, an index that cannot be negative -- and each one is bounded by the
// field that produced it. Blanket-denying them would mean a `try_from` and an
// `unwrap_or` on every line of the wavelet.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
//! A JPEG 2000 decoder, written from ISO/IEC 15444-1 (Part 1) and depending on
//! nothing.
//!
//! # What it decodes
//!
//! Whole Part 1 codestreams: tiles and tile-parts in any order, all five
//! progression orders, precincts, every code-block style, both wavelet filters,
//! all three quantisation styles, regions of interest, subsampled components,
//! and any bit depth up to 31 with either sign. It also reads the JP2 wrapper
//! a `.jp2` file puts around a codestream, including its palette and channel
//! definitions.
//!
//! It does not decode Part 2 extensions. A Part 2 file whose codestream is
//! plain Part 1 -- which is what most of them are -- decodes normally.
//!
//! # Strict and forgiving
//!
//! [`decode`] refuses a codestream that breaks any rule of the standard.
//! [`decode_recovering`] decodes it anyway and returns a list of
//! [`Repair`]s saying which rules were broken. Neither one guesses silently:
//! the difference between the two is always visible in the result.
//!
//! ```no_run
//! # fn main() -> Result<(), jpeg2000::Error> {
//! let bytes = std::fs::read("image.jp2").unwrap();
//! let image = jpeg2000::decode_recovering(&bytes)?;
//! for repair in &image.repairs {
//!     eprintln!("forgave: {repair}");
//! }
//! println!("{}x{}, {} components", image.width, image.height, image.components.len());
//! # Ok(())
//! # }
//! ```

mod bits;
mod codestream;
mod decode;
mod dwt;
mod error;
mod jp2;
mod mq;
mod t1;
mod tile;

pub use decode::{Component, Image};
pub use error::{Error, ErrorKind, Limits, Repair};
pub use jp2::ColourSpace;

/// Decodes a codestream or a JP2 file, refusing anything malformed.
///
/// # Errors
///
/// Returns the first rule of the standard the input breaks.
pub fn decode(bytes: &[u8]) -> Result<Image, Error> {
    decode_with(bytes, false, Limits::default())
}

/// [`decode`], with a ceiling of your own on what it will allocate.
///
/// # Errors
///
/// The same as [`decode`], plus [`ErrorKind::TooLarge`] for an image the limits
/// do not allow.
pub fn decode_with_limits(bytes: &[u8], limits: Limits) -> Result<Image, Error> {
    decode_with(bytes, false, limits)
}

/// Decodes a codestream or a JP2 file, working around the rules that real
/// encoders break and reporting each one in [`Image::repairs`].
///
/// # Errors
///
/// Returns an error for the damage this decoder cannot work around, which is
/// anything that leaves it without a coherent image geometry.
pub fn decode_recovering(bytes: &[u8]) -> Result<Image, Error> {
    decode_with(bytes, true, Limits::default())
}

/// [`decode_recovering`], with a ceiling of your own on what it will allocate.
///
/// # Errors
///
/// The same as [`decode_recovering`], plus [`ErrorKind::TooLarge`] for an image
/// the limits do not allow.
pub fn decode_recovering_with_limits(bytes: &[u8], limits: Limits) -> Result<Image, Error> {
    decode_with(bytes, true, limits)
}

fn decode_with(bytes: &[u8], forgiving: bool, limits: Limits) -> Result<Image, Error> {
    let (range, _container) =
        jp2::find_codestream(bytes).ok_or_else(|| Error::at(0, ErrorKind::NotACodestream))?;
    let data = bytes.get(range.clone()).unwrap_or(&[]);
    let parsed = codestream::parse(data, forgiving)
        .map_err(|error| Error::at(error.offset + range.start, error.kind))?;
    decode::decode(&parsed, data, limits)
}
