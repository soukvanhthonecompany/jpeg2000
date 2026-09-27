//! What a codestream can be refused for, and what it can be forgiven.
//!
//! The two are separate types on purpose. A decoder that quietly repairs its
//! input is a decoder whose output cannot be trusted to mean anything, and one
//! that refuses every imperfect file cannot read the files that exist. So the
//! strict entry point returns [`Error`] where the recovering one returns a
//! [`Repair`] alongside the image, and a caller can always tell which happened.

use core::fmt;

/// How much a decoder is willing to allocate on a codestream's say-so.
///
/// Every size in a codestream is a declaration, and a declaration is not a
/// promise: four bytes of image width can ask for more memory than the machine
/// has before a single sample has been decoded. These are the ceilings, and
/// they are a parameter rather than a constant because what is absurd for a
/// thumbnail is ordinary for a scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Limits {
    /// Most samples an image may hold, summed over every component.
    pub max_samples: usize,
    /// Most coefficients one tile component may hold, summed over every
    /// subband of every resolution.
    pub max_tile_coefficients: usize,
}

impl Limits {
    /// Ceilings of your own. The struct is `non_exhaustive` so that a later
    /// version can add a limit without breaking callers, which means this is
    /// how one is built with anything other than the defaults.
    #[must_use]
    pub const fn new(max_samples: usize, max_tile_coefficients: usize) -> Self {
        Self {
            max_samples,
            max_tile_coefficients,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        // Sixty-seven million samples is a 4730x4730 image in three components,
        // and a quarter of a gigabyte once decoded. Bigger than anything a page
        // holds, small enough that a malformed header cannot end the process.
        Self {
            max_samples: 1 << 26,
            max_tile_coefficients: 1 << 26,
        }
    }
}

/// Why a codestream could not be decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    /// Where in the codestream the decoder was when it gave up.
    pub offset: usize,
    pub kind: ErrorKind,
}

impl Error {
    pub(crate) const fn at(offset: usize, kind: ErrorKind) -> Self {
        Self { offset, kind }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// No start-of-codestream marker, and no JP2 box holding one.
    NotACodestream,
    /// A marker segment claims more bytes than the codestream has.
    TruncatedMarkerSegment,
    /// A marker segment's length disagrees with the fields it must contain.
    MalformedMarkerSegment,
    /// A marker that is not part of this standard, where one is required.
    UnknownMarker(u16),
    /// The image header is missing, repeated, or does not come first.
    MalformedImageHeader,
    /// A tile or tile-component has no coding style or no quantisation.
    MissingCodingStyle,
    /// A tile index no tile grid could produce.
    TileIndexOutOfRange(u16),
    /// A component index past the number the image declares.
    ComponentIndexOutOfRange(u16),
    /// A progression order this standard does not define.
    UnknownProgression(u8),
    /// A wavelet transform this standard does not define.
    UnknownTransform(u8),
    /// A quantisation style this standard does not define.
    UnknownQuantisation(u8),
    /// An image with no components, or no area.
    EmptyImage,
    /// A field whose value is legal but larger than this decoder will allocate.
    TooLarge,
    /// The codestream stops in the middle of something it had started.
    UnexpectedEndOfCodestream,
    /// A tile-part header in a place the standard does not allow one.
    MisplacedTilePart,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at byte {}: {}", self.offset, self.kind)
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ErrorKind::NotACodestream => formatter.write_str("not a JPEG 2000 codestream"),
            ErrorKind::TruncatedMarkerSegment => {
                formatter.write_str("a marker segment runs past the end of the codestream")
            }
            ErrorKind::MalformedMarkerSegment => {
                formatter.write_str("a marker segment's length does not match its contents")
            }
            ErrorKind::UnknownMarker(marker) => {
                write!(formatter, "unknown marker 0x{marker:04x}")
            }
            ErrorKind::MalformedImageHeader => formatter.write_str("malformed image header"),
            ErrorKind::MissingCodingStyle => {
                formatter.write_str("a tile has no coding style or no quantisation")
            }
            ErrorKind::TileIndexOutOfRange(index) => {
                write!(formatter, "tile index {index} is outside the tile grid")
            }
            ErrorKind::ComponentIndexOutOfRange(index) => {
                write!(formatter, "component index {index} is outside the image")
            }
            ErrorKind::UnknownProgression(value) => {
                write!(formatter, "unknown progression order {value}")
            }
            ErrorKind::UnknownTransform(value) => {
                write!(formatter, "unknown wavelet transform {value}")
            }
            ErrorKind::UnknownQuantisation(value) => {
                write!(formatter, "unknown quantisation style {value}")
            }
            ErrorKind::EmptyImage => formatter.write_str("the image has no samples"),
            ErrorKind::TooLarge => {
                formatter.write_str("the image is larger than this decoder will allocate")
            }
            ErrorKind::UnexpectedEndOfCodestream => {
                formatter.write_str("the codestream ends in the middle of a segment")
            }
            ErrorKind::MisplacedTilePart => formatter.write_str("a tile-part header is misplaced"),
        }
    }
}

impl core::error::Error for Error {}

/// A rule a codestream broke that decoding survived.
///
/// Every one of these is a thing some encoder actually writes. They are
/// returned rather than logged so that a caller can refuse a file its own
/// users would call broken, without this decoder having to guess.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Repair {
    /// A tile-part header declared a number of tile-parts that does not match
    /// how many the codestream actually carries for that tile.
    ///
    /// Written by at least one widely used encoder. The field is an index, not
    /// a payload: every tile-part is present and in order, and the count is
    /// simply wrong, so decoding uses what is there.
    TilePartCountDisagrees {
        tile: u16,
        declared: u16,
        found: u16,
    },
    /// A tile-part's declared length runs past the end of the codestream. The
    /// tile-part is decoded as far as the bytes go.
    TilePartRunsPastTheEnd { tile: u16 },
    /// A tile-part declared its length as zero, which B.9 allows only for the
    /// last tile-part in the codestream. It was read to the next marker.
    TilePartLengthUnstated { tile: u16 },
    /// The codestream has no end-of-codestream marker.
    EndOfCodestreamMissing,
    /// A packet header asked for more bytes than the tile's data holds. The
    /// packets that were complete are kept.
    PacketsRunPastTheirTile { tile: u16 },
    /// A code-block's segmentation symbol did not read back, so some part of
    /// that block's coefficients is wrong. The block is kept as decoded.
    CodeBlockDamaged { tile: u16, component: u16 },
    /// A marker this standard does not define was found between segments and
    /// skipped.
    UnknownMarkerSkipped { marker: u16 },
}

impl fmt::Display for Repair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::TilePartCountDisagrees {
                tile,
                declared,
                found,
            } => write!(
                formatter,
                "tile {tile} declares {declared} tile-parts and carries {found}"
            ),
            Self::TilePartRunsPastTheEnd { tile } => {
                write!(formatter, "tile {tile} has a tile-part past the end")
            }
            Self::TilePartLengthUnstated { tile } => {
                write!(formatter, "tile {tile} has a tile-part of unstated length")
            }
            Self::EndOfCodestreamMissing => formatter.write_str("no end-of-codestream marker"),
            Self::PacketsRunPastTheirTile { tile } => {
                write!(
                    formatter,
                    "tile {tile} has packets past the end of its data"
                )
            }
            Self::CodeBlockDamaged { tile, component } => write!(
                formatter,
                "tile {tile} component {component} has a damaged code-block"
            ),
            Self::UnknownMarkerSkipped { marker } => {
                write!(formatter, "skipped unknown marker 0x{marker:04x}")
            }
        }
    }
}
