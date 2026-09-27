//! Marker segments, and the walk that finds them. ISO/IEC 15444-1 Annex A.
//!
//! A codestream is a main header, then tile-parts, then an end marker. Every
//! header is a list of marker segments, and the coding style and quantisation
//! a tile uses are whatever the main header said unless that tile's own header
//! says otherwise -- so the parse is: read the main header into a default, then
//! clone that default per tile and let the tile's header overwrite parts of it.

use crate::error::{Error, ErrorKind, Repair};

pub const SOC: u16 = 0xFF4F;
pub const SIZ: u16 = 0xFF51;
pub const COD: u16 = 0xFF52;
pub const COC: u16 = 0xFF53;
pub const TLM: u16 = 0xFF55;
pub const PLM: u16 = 0xFF57;
pub const PLT: u16 = 0xFF58;
pub const QCD: u16 = 0xFF5C;
pub const QCC: u16 = 0xFF5D;
pub const RGN: u16 = 0xFF5E;
pub const POC: u16 = 0xFF5F;
pub const PPM: u16 = 0xFF60;
pub const PPT: u16 = 0xFF61;
pub const CRG: u16 = 0xFF63;
pub const COM: u16 = 0xFF64;
pub const SOT: u16 = 0xFF90;
pub const SOP: u16 = 0xFF91;
pub const EPH: u16 = 0xFF92;
pub const SOD: u16 = 0xFF93;
pub const EOC: u16 = 0xFFD9;

/// How the packets of a tile are ordered, B.12.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progression {
    LayerResolutionComponentPosition,
    ResolutionLayerComponentPosition,
    ResolutionPositionComponentLayer,
    PositionComponentResolutionLayer,
    ComponentPositionResolutionLayer,
}

impl Progression {
    const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::LayerResolutionComponentPosition,
            1 => Self::ResolutionLayerComponentPosition,
            2 => Self::ResolutionPositionComponentLayer,
            3 => Self::PositionComponentResolutionLayer,
            4 => Self::ComponentPositionResolutionLayer,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transform {
    /// The 9/7 filter: lossy, and the coefficients are quantised.
    Irreversible,
    /// The 5/3 filter: integer in and integer out, so lossless is possible.
    Reversible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Quantisation {
    /// No quantisation at all; the step sizes carry only exponents.
    None,
    /// One step size, from which every other subband's is derived.
    Derived,
    /// One step size per subband, written out.
    Expounded,
}

/// One component's place on the reference grid, A.5.1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentSpec {
    pub depth: u8,
    pub signed: bool,
    pub dx: u32,
    pub dy: u32,
}

/// The image header, A.5.1.
#[derive(Clone, Debug)]
pub struct ImageHeader {
    pub x1: u32,
    pub y1: u32,
    pub x0: u32,
    pub y0: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub tile_x0: u32,
    pub tile_y0: u32,
    pub components: Vec<ComponentSpec>,
}

impl ImageHeader {
    #[must_use]
    pub const fn tiles_wide(&self) -> u32 {
        (self.x1 - self.tile_x0).div_ceil(self.tile_width)
    }

    #[must_use]
    pub const fn tiles_high(&self) -> u32 {
        (self.y1 - self.tile_y0).div_ceil(self.tile_height)
    }
}

/// The part of a coding style that can be set per component, A.6.1.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockCoding {
    pub levels: u8,
    /// The code-block width and height, as powers of two.
    pub block_width: u8,
    pub block_height: u8,
    pub style: u8,
    pub transform: Transform,
    /// One `(PPx, PPy)` per resolution level, lowest first. Always populated:
    /// a codestream that declares none gets the maximum, which is what a
    /// default precinct is.
    pub precincts: Vec<(u8, u8)>,
}

/// A tile's coding style: the per-component part, plus what only the tile has.
#[derive(Clone, Debug)]
pub struct Coding {
    pub progression: Progression,
    pub layers: u16,
    /// Whether the three first components are transformed together.
    pub multiple_component_transform: bool,
    pub default: BlockCoding,
    /// Per-component overrides from a `COC` segment.
    pub components: Vec<Option<BlockCoding>>,
    pub start_of_packet: bool,
    pub end_of_packet_header: bool,
}

impl Coding {
    #[must_use]
    pub fn for_component(&self, component: usize) -> &BlockCoding {
        self.components
            .get(component)
            .and_then(Option::as_ref)
            .unwrap_or(&self.default)
    }
}

/// One subband's step size, A.6.4.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StepSize {
    pub exponent: u8,
    pub mantissa: u16,
}

#[derive(Clone, Debug)]
pub struct Quant {
    pub style: Quantisation,
    pub guard_bits: u8,
    pub steps: Vec<StepSize>,
}

/// Everything a tile needs in order to be decoded.
#[derive(Clone, Debug)]
pub struct TileStyle {
    pub coding: Coding,
    /// Quantisation per component; index zero is also the default.
    pub quant: Vec<Quant>,
    /// Region-of-interest shift per component, A.6.3.
    pub roi_shift: Vec<u8>,
}

/// One tile-part's bitstream, already located.
#[derive(Clone, Debug)]
pub struct TilePart {
    pub tile: u16,
    pub index: u8,
    pub start: usize,
    pub end: usize,
}

/// A parsed codestream: the headers, and where every tile's bits are.
pub struct Codestream {
    pub image: ImageHeader,
    pub tiles: Vec<TileStyle>,
    pub parts: Vec<TilePart>,
    pub repairs: Vec<Repair>,
}

struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn u8(&mut self) -> Result<u8, Error> {
        let value = *self
            .data
            .get(self.offset)
            .ok_or_else(|| Error::at(self.offset, ErrorKind::UnexpectedEndOfCodestream))?;
        self.offset += 1;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok((u16::from(self.u8()?) << 8) | u16::from(self.u8()?))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok((u32::from(self.u16()?) << 16) | u32::from(self.u16()?))
    }
}

/// Reads one `SIZ` segment, A.5.1.
fn image_header(segment: &[u8], offset: usize) -> Result<ImageHeader, Error> {
    let mut reader = Reader::new(segment);
    let malformed = || Error::at(offset, ErrorKind::MalformedImageHeader);
    let _capabilities = reader.u16().map_err(|_| malformed())?;
    let x1 = reader.u32().map_err(|_| malformed())?;
    let y1 = reader.u32().map_err(|_| malformed())?;
    let x0 = reader.u32().map_err(|_| malformed())?;
    let y0 = reader.u32().map_err(|_| malformed())?;
    let tile_width = reader.u32().map_err(|_| malformed())?;
    let tile_height = reader.u32().map_err(|_| malformed())?;
    let tile_x0 = reader.u32().map_err(|_| malformed())?;
    let tile_y0 = reader.u32().map_err(|_| malformed())?;
    let count = reader.u16().map_err(|_| malformed())?;
    if x1 <= x0 || y1 <= y0 || count == 0 {
        return Err(Error::at(offset, ErrorKind::EmptyImage));
    }
    if tile_width == 0 || tile_height == 0 || tile_x0 > x0 || tile_y0 > y0 {
        return Err(malformed());
    }
    let mut components = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let precision = reader.u8().map_err(|_| malformed())?;
        let dx = u32::from(reader.u8().map_err(|_| malformed())?);
        let dy = u32::from(reader.u8().map_err(|_| malformed())?);
        // The standard allows up to 38 bits per sample. This decoder hands
        // samples back as `i32`, so it refuses what it could not represent
        // rather than truncating it silently.
        if dx == 0 || dy == 0 {
            return Err(malformed());
        }
        if (precision & 0x7F) + 1 > 31 {
            return Err(Error::at(offset, ErrorKind::TooLarge));
        }
        components.push(ComponentSpec {
            depth: (precision & 0x7F) + 1,
            signed: precision & 0x80 != 0,
            dx,
            dy,
        });
    }
    Ok(ImageHeader {
        x1,
        y1,
        x0,
        y0,
        tile_width,
        tile_height,
        tile_x0,
        tile_y0,
        components,
    })
}

/// The part of `COD` and `COC` that describes code-blocks and the transform.
fn block_coding(
    reader: &mut Reader<'_>,
    has_precincts: bool,
    offset: usize,
) -> Result<BlockCoding, Error> {
    let malformed = || Error::at(offset, ErrorKind::MalformedMarkerSegment);
    let levels = reader.u8().map_err(|_| malformed())?;
    let block_width = (reader.u8().map_err(|_| malformed())? & 0x0F) + 2;
    let block_height = (reader.u8().map_err(|_| malformed())? & 0x0F) + 2;
    let style = reader.u8().map_err(|_| malformed())?;
    let transform = match reader.u8().map_err(|_| malformed())? {
        0 => Transform::Irreversible,
        1 => Transform::Reversible,
        other => return Err(Error::at(offset, ErrorKind::UnknownTransform(other))),
    };
    if levels > 32 || block_width > 10 || block_height > 10 || block_width + block_height > 12 {
        return Err(Error::at(offset, ErrorKind::TooLarge));
    }
    let wanted = levels as usize + 1;
    let mut precincts = Vec::with_capacity(wanted);
    if has_precincts {
        for index in 0..wanted {
            let byte = reader.u8().map_err(|_| malformed())?;
            let (ppx, ppy) = (byte & 0x0F, byte >> 4);
            // B.6: only the lowest resolution may have a precinct one sample
            // wide. Above it the precinct partition is carried onto a subband
            // one level down, and an exponent of zero has nowhere to go.
            if index > 0 && (ppx == 0 || ppy == 0) {
                return Err(malformed());
            }
            precincts.push((ppx, ppy));
        }
    } else {
        // B.6: with no precinct sizes declared, every precinct is as large as
        // the standard allows, which is the same as saying there is one.
        precincts.resize(wanted, (15, 15));
    }
    Ok(BlockCoding {
        levels,
        block_width,
        block_height,
        style,
        transform,
        precincts,
    })
}

/// Reads one `COD` segment, A.6.1.
fn coding_style(segment: &[u8], offset: usize, components: usize) -> Result<Coding, Error> {
    let mut reader = Reader::new(segment);
    let malformed = || Error::at(offset, ErrorKind::MalformedMarkerSegment);
    let scod = reader.u8().map_err(|_| malformed())?;
    let progression_code = reader.u8().map_err(|_| malformed())?;
    let progression = Progression::from_code(progression_code)
        .ok_or_else(|| Error::at(offset, ErrorKind::UnknownProgression(progression_code)))?;
    let layers = reader.u16().map_err(|_| malformed())?;
    let multiple_component_transform = reader.u8().map_err(|_| malformed())? != 0;
    let default = block_coding(&mut reader, scod & 1 != 0, offset)?;
    if layers == 0 {
        return Err(malformed());
    }
    Ok(Coding {
        progression,
        layers,
        multiple_component_transform,
        default,
        components: vec![None; components],
        start_of_packet: scod & 2 != 0,
        end_of_packet_header: scod & 4 != 0,
    })
}

/// Reads one `QCD` or `QCC` step-size list, A.6.4.
fn quantisation(segment: &[u8], offset: usize) -> Result<Quant, Error> {
    let mut reader = Reader::new(segment);
    let malformed = || Error::at(offset, ErrorKind::MalformedMarkerSegment);
    let sq = reader.u8().map_err(|_| malformed())?;
    let guard_bits = sq >> 5;
    let style = match sq & 0x1F {
        0 => Quantisation::None,
        1 => Quantisation::Derived,
        2 => Quantisation::Expounded,
        other => return Err(Error::at(offset, ErrorKind::UnknownQuantisation(other))),
    };
    let mut steps = Vec::new();
    if style == Quantisation::None {
        while let Ok(byte) = reader.u8() {
            steps.push(StepSize {
                exponent: byte >> 3,
                mantissa: 0,
            });
        }
    } else {
        while let Ok(value) = reader.u16() {
            steps.push(StepSize {
                exponent: (value >> 11) as u8,
                mantissa: value & 0x7FF,
            });
        }
    }
    if steps.is_empty() {
        return Err(malformed());
    }
    Ok(Quant {
        style,
        guard_bits,
        steps,
    })
}

/// A component index, one byte when the image has fewer than 257 components
/// and two when it has more. A.6.2.
fn component_index(
    reader: &mut Reader<'_>,
    components: usize,
    offset: usize,
) -> Result<usize, Error> {
    let malformed = || Error::at(offset, ErrorKind::MalformedMarkerSegment);
    let index = if components < 257 {
        usize::from(reader.u8().map_err(|_| malformed())?)
    } else {
        usize::from(reader.u16().map_err(|_| malformed())?)
    };
    if index >= components {
        let narrowed = u16::try_from(index).unwrap_or(u16::MAX);
        return Err(Error::at(
            offset,
            ErrorKind::ComponentIndexOutOfRange(narrowed),
        ));
    }
    Ok(index)
}

/// Applies one marker segment to a style, whether the main header's or a
/// tile's. The two headers accept the same segments; the difference is only
/// which style they change.
fn apply(
    style: &mut TileStyle,
    marker: u16,
    segment: &[u8],
    offset: usize,
    components: usize,
) -> Result<(), Error> {
    match marker {
        COD => {
            let coding = coding_style(segment, offset, components)?;
            // A tile's own COD replaces the defaults but not the per-component
            // overrides a COC in the same header may already have set.
            let existing = std::mem::take(&mut style.coding.components);
            style.coding = coding;
            if existing.iter().any(Option::is_some) {
                style.coding.components = existing;
            }
        }
        COC => {
            let mut reader = Reader::new(segment);
            let index = component_index(&mut reader, components, offset)?;
            let scoc = reader
                .u8()
                .map_err(|_| Error::at(offset, ErrorKind::MalformedMarkerSegment))?;
            let block = block_coding(&mut reader, scoc & 1 != 0, offset)?;
            style.coding.components[index] = Some(block);
        }
        QCD => {
            let quant = quantisation(segment, offset)?;
            style.quant = vec![quant; components];
        }
        QCC => {
            let mut reader = Reader::new(segment);
            let index = component_index(&mut reader, components, offset)?;
            let rest = &segment[reader.offset..];
            style.quant[index] = quantisation(rest, offset)?;
        }
        RGN => {
            let mut reader = Reader::new(segment);
            let index = component_index(&mut reader, components, offset)?;
            let _style_of_roi = reader.u8();
            style.roi_shift[index] = reader
                .u8()
                .map_err(|_| Error::at(offset, ErrorKind::MalformedMarkerSegment))?;
        }
        // Read and ignored: comments, packed packet headers we do not need
        // because the packets themselves carry them, and the index markers
        // that only make random access faster.
        COM | TLM | PLM | PLT | CRG | POC | PPM | PPT => {}
        _ => return Err(Error::at(offset, ErrorKind::UnknownMarker(marker))),
    }
    Ok(())
}

/// The two bytes at `offset`, if there are two.
fn marker_at(data: &[u8], offset: usize) -> Option<u16> {
    data.get(offset..offset + 2)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// The style every tile starts from: the standard's defaults, before the main
/// header has said anything. A codestream always overrides these, but a tile
/// whose header is damaged is decoded against something rather than nothing.
fn default_style(components: usize) -> TileStyle {
    TileStyle {
        coding: Coding {
            progression: Progression::LayerResolutionComponentPosition,
            layers: 1,
            multiple_component_transform: false,
            default: BlockCoding {
                levels: 5,
                block_width: 6,
                block_height: 6,
                style: 0,
                transform: Transform::Reversible,
                precincts: vec![(15, 15); 6],
            },
            components: vec![None; components],
            start_of_packet: false,
            end_of_packet_header: false,
        },
        quant: vec![
            Quant {
                style: Quantisation::None,
                guard_bits: 2,
                steps: vec![StepSize::default()],
            };
            components
        ],
        roi_shift: vec![0; components],
    }
}

/// Reads the main header: every marker segment before the first tile-part.
///
/// Returns the image header, the style every tile inherits, and where the
/// tile-parts begin.
fn parse_main_header(
    data: &[u8],
    forgiving: bool,
    repairs: &mut Vec<Repair>,
) -> Result<(ImageHeader, TileStyle, usize, bool), Error> {
    if data.len() < 4 || marker_at(data, 0) != Some(SOC) {
        return Err(Error::at(0, ErrorKind::NotACodestream));
    }
    let mut offset = 2;
    if marker_at(data, offset) != Some(SIZ) {
        return Err(Error::at(offset, ErrorKind::MalformedImageHeader));
    }

    let mut image = None;
    let mut main: Option<TileStyle> = None;
    let mut components = 0usize;
    let mut saw_end = false;
    while offset + 2 <= data.len() {
        let Some(marker) = marker_at(data, offset) else {
            break;
        };
        if marker == EOC {
            saw_end = true;
            break;
        }
        if marker == SOT {
            break;
        }
        let length = usize::from(
            marker_at(data, offset + 2)
                .ok_or(Error::at(offset, ErrorKind::TruncatedMarkerSegment))?,
        );
        if length < 2 || offset + 2 + length > data.len() {
            return Err(Error::at(offset, ErrorKind::TruncatedMarkerSegment));
        }
        let segment = &data[offset + 4..offset + 2 + length];
        if marker == SIZ {
            if image.is_some() {
                return Err(Error::at(offset, ErrorKind::MalformedImageHeader));
            }
            let header = image_header(segment, offset)?;
            components = header.components.len();
            main = Some(default_style(components));
            image = Some(header);
        } else {
            let style = main
                .as_mut()
                .ok_or(Error::at(offset, ErrorKind::MalformedImageHeader))?;
            apply_or_skip(
                style, marker, segment, offset, components, forgiving, repairs,
            )?;
        }
        offset += 2 + length;
    }

    match (image, main) {
        (Some(image), Some(main)) => Ok((image, main, offset, saw_end)),
        _ => Err(Error::at(offset, ErrorKind::MalformedImageHeader)),
    }
}

/// Applies a marker segment, forgiving one this standard does not define.
fn apply_or_skip(
    style: &mut TileStyle,
    marker: u16,
    segment: &[u8],
    offset: usize,
    components: usize,
    forgiving: bool,
    repairs: &mut Vec<Repair>,
) -> Result<(), Error> {
    match apply(style, marker, segment, offset, components) {
        Ok(()) => Ok(()),
        Err(error) if forgiving && matches!(error.kind, ErrorKind::UnknownMarker(_)) => {
            repairs.push(Repair::UnknownMarkerSkipped { marker });
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Where one tile-part's bitstream ends, B.9.
fn tile_part_end(
    data: &[u8],
    offset: usize,
    tile: u16,
    declared_length: usize,
    forgiving: bool,
    repairs: &mut Vec<Repair>,
) -> Result<usize, Error> {
    if declared_length == 0 {
        // A zero length means "to the end of the codestream", which B.9 allows
        // only for the last tile-part.
        if !forgiving {
            return Err(Error::at(offset, ErrorKind::MalformedMarkerSegment));
        }
        repairs.push(Repair::TilePartLengthUnstated { tile });
        return Ok(data.len());
    }
    if offset + declared_length > data.len() {
        if !forgiving {
            return Err(Error::at(offset, ErrorKind::TruncatedMarkerSegment));
        }
        repairs.push(Repair::TilePartRunsPastTheEnd { tile });
        return Ok(data.len());
    }
    Ok(offset + declared_length)
}

/// Reads a tile-part header, from just after its `SOT` segment to its `SOD`.
///
/// Only the first tile-part of a tile may change its style, A.4.2, but a later
/// one that tries is applied rather than refused: applying it is what the
/// encoder meant, and refusing it would lose a whole tile over a field nothing
/// else reads.
///
/// Returns the offset of the first byte of the tile-part's bitstream.
fn read_tile_part_header(
    data: &[u8],
    mut cursor: usize,
    style: &mut TileStyle,
    components: usize,
    forgiving: bool,
    repairs: &mut Vec<Repair>,
) -> Result<usize, Error> {
    loop {
        let Some(marker) = marker_at(data, cursor) else {
            return Err(Error::at(cursor, ErrorKind::UnexpectedEndOfCodestream));
        };
        if marker == SOD {
            return Ok(cursor + 2);
        }
        let length = usize::from(
            marker_at(data, cursor + 2)
                .ok_or(Error::at(cursor, ErrorKind::TruncatedMarkerSegment))?,
        );
        if length < 2 || cursor + 2 + length > data.len() {
            return Err(Error::at(cursor, ErrorKind::TruncatedMarkerSegment));
        }
        let segment = &data[cursor + 4..cursor + 2 + length];
        apply_or_skip(
            style, marker, segment, cursor, components, forgiving, repairs,
        )?;
        cursor += 2 + length;
    }
}

/// Compares what each tile said it would carry with what it carried.
fn reconcile_tile_part_counts(
    declared_parts: &[Option<u16>],
    found_parts: &[u16],
    forgiving: bool,
    repairs: &mut Vec<Repair>,
) -> Result<(), Error> {
    for (index, found) in found_parts.iter().enumerate() {
        let Some(declared) = declared_parts[index] else {
            continue;
        };
        if declared == *found {
            continue;
        }
        if !forgiving {
            return Err(Error::at(0, ErrorKind::MalformedMarkerSegment));
        }
        repairs.push(Repair::TilePartCountDisagrees {
            tile: u16::try_from(index).unwrap_or(u16::MAX),
            declared,
            found: *found,
        });
    }
    Ok(())
}

/// Parses a whole codestream: headers, and where each tile-part's bits begin.
///
/// `forgiving` decides what happens to the rules real encoders break. With it
/// off, any of them is an [`Error`]; with it on, each is a [`Repair`] and
/// decoding continues.
///
/// # Errors
///
/// Returns the first rule the codestream breaks that this decoder will not
/// work around.
pub fn parse(data: &[u8], forgiving: bool) -> Result<Codestream, Error> {
    let mut repairs = Vec::new();
    let (image, main, mut offset, mut saw_end) = parse_main_header(data, forgiving, &mut repairs)?;
    let components = image.components.len();
    let tile_count = (image.tiles_wide() as usize) * (image.tiles_high() as usize);
    if tile_count == 0 || tile_count > 1 << 16 {
        return Err(Error::at(offset, ErrorKind::TooLarge));
    }
    let mut tiles: Vec<TileStyle> = vec![main; tile_count];
    let mut declared_parts: Vec<Option<u16>> = vec![None; tile_count];
    let mut found_parts: Vec<u16> = vec![0; tile_count];
    let mut parts: Vec<TilePart> = Vec::new();

    while offset + 2 <= data.len() {
        let Some(marker) = marker_at(data, offset) else {
            break;
        };
        if marker == EOC {
            saw_end = true;
            break;
        }
        if marker != SOT {
            return Err(Error::at(offset, ErrorKind::MisplacedTilePart));
        }
        let segment_length = usize::from(
            marker_at(data, offset + 2)
                .ok_or(Error::at(offset, ErrorKind::TruncatedMarkerSegment))?,
        );
        if segment_length < 10 || offset + 2 + segment_length > data.len() {
            return Err(Error::at(offset, ErrorKind::TruncatedMarkerSegment));
        }
        let mut reader = Reader::new(&data[offset + 4..offset + 2 + segment_length]);
        let tile = reader.u16()?;
        let declared_length = reader.u32()? as usize;
        let part_index = reader.u8()?;
        let declared = reader.u8()?;
        let index = usize::from(tile);
        if index >= tile_count {
            return Err(Error::at(offset, ErrorKind::TileIndexOutOfRange(tile)));
        }
        let part_end = tile_part_end(data, offset, tile, declared_length, forgiving, &mut repairs)?;
        if declared != 0 {
            let previous = declared_parts[index].replace(u16::from(declared));
            if let Some(previous) = previous
                && previous != u16::from(declared)
                && !forgiving
            {
                return Err(Error::at(offset, ErrorKind::MalformedMarkerSegment));
            }
        }
        found_parts[index] += 1;

        let cursor = read_tile_part_header(
            data,
            offset + 2 + segment_length,
            &mut tiles[index],
            components,
            forgiving,
            &mut repairs,
        )?;

        parts.push(TilePart {
            tile,
            index: part_index,
            start: cursor,
            end: part_end.max(cursor),
        });
        offset = part_end;
    }

    reconcile_tile_part_counts(&declared_parts, &found_parts, forgiving, &mut repairs)?;
    if !saw_end {
        if !forgiving {
            return Err(Error::at(data.len(), ErrorKind::UnexpectedEndOfCodestream));
        }
        repairs.push(Repair::EndOfCodestreamMissing);
    }

    // Tile-parts are decoded in the order they were written, which for one tile
    // is their index order. A codestream may interleave tiles freely.
    parts.sort_by_key(|part| (part.tile, part.index));
    Ok(Codestream {
        image,
        tiles,
        parts,
        repairs,
    })
}
