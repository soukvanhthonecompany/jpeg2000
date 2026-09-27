//! A tile's geometry, and the packets that fill it. Annexes B and E.
//!
//! Everything below is coordinates. A tile is cut out of the reference grid, a
//! component is the tile sampled, a resolution is that component halved some
//! number of times, a subband is one quarter of a resolution, a precinct is a
//! rectangle of a resolution, and a code-block is a rectangle of a subband
//! inside a precinct. Each of those rectangles is defined by rounding the one
//! above it, and the rounding is always *up* on the low edge -- which is why
//! all of it is written out rather than inferred from sizes.

use crate::bits::{BitReader, TagTree};
use crate::codestream::{
    self, BlockCoding, Coding, ImageHeader, Progression, Quant, Quantisation, StepSize, TileStyle,
};
use crate::error::Limits;
use crate::t1::{self, BandKind, Segment};

pub struct Band {
    pub kind: BandKind,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub step: StepSize,
    /// The band's nominal dynamic range, `R_b` of E.1.
    pub range: u32,
    pub bitplanes: u32,
    pub coefficients: Vec<i32>,
    pub precincts: Vec<Precinct>,
}

impl Band {
    pub const fn width(&self) -> u32 {
        self.x1 - self.x0
    }

    pub const fn height(&self) -> u32 {
        self.y1 - self.y0
    }

    const fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
}

pub struct Precinct {
    pub blocks_wide: u32,
    pub blocks: Vec<CodeBlock>,
    inclusion: TagTree,
    missing_bitplanes: TagTree,
}

pub struct CodeBlock {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub included: bool,
    pub lblock: u32,
    pub zero_bitplanes: u32,
    pub passes: u32,
    pub data: Vec<u8>,
    pub segments: Vec<Segment>,
}

pub struct Resolution {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    /// The precinct partition exponents at this resolution.
    pub ppx: u8,
    pub ppy: u8,
    pub precincts_wide: u32,
    pub precincts_high: u32,
    pub bands: Vec<Band>,
}

impl Resolution {
    pub const fn precinct_count(&self) -> u32 {
        self.precincts_wide * self.precincts_high
    }
}

pub struct TileComponent {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub levels: u8,
    pub resolutions: Vec<Resolution>,
}

impl TileComponent {
    pub const fn width(&self) -> u32 {
        self.x1 - self.x0
    }

    pub const fn height(&self) -> u32 {
        self.y1 - self.y0
    }
}

/// `ceil(value / 2^shift)`, which is the only rounding this annex ever does.
const fn ceil_shift(value: u32, shift: u32) -> u32 {
    if shift >= 32 {
        if value == 0 { 0 } else { 1 }
    } else {
        let mask = (1u32 << shift) - 1;
        (value >> shift) + if value & mask == 0 { 0 } else { 1 }
    }
}

/// Which step size a band uses, and how many bit-planes its coefficients have.
fn band_step(
    quant: &Quant,
    levels: u8,
    resolution: usize,
    band_in_resolution: usize,
    depth: u8,
    gain: u32,
) -> (StepSize, u32, u32) {
    let index = if resolution == 0 {
        0
    } else {
        3 * (resolution - 1) + band_in_resolution + 1
    };
    let step = match quant.style {
        Quantisation::Derived => {
            let first = quant.steps.first().copied().unwrap_or_default();
            // E.1.1: one step size is written and the rest are implied by how
            // many times the band has been decomposed.
            let decomposition = if resolution == 0 {
                u32::from(levels)
            } else {
                u32::from(levels) - u32::try_from(resolution).unwrap_or(0) + 1
            };
            let shift = u32::from(levels) - decomposition;
            StepSize {
                exponent: first
                    .exponent
                    .saturating_sub(u8::try_from(shift).unwrap_or(u8::MAX)),
                mantissa: first.mantissa,
            }
        }
        Quantisation::None | Quantisation::Expounded => quant
            .steps
            .get(index)
            .copied()
            .unwrap_or_else(|| quant.steps.last().copied().unwrap_or_default()),
    };
    let range = u32::from(depth) + gain;
    // E.1: the number of magnitude bits a coefficient of this band can have.
    // A codestream that declares no guard bits and a zero exponent declares a
    // band with no magnitude at all, which decodes to zeroes rather than to a
    // refusal.
    let bitplanes = (u32::from(quant.guard_bits) + u32::from(step.exponent)).saturating_sub(1);
    (step, range, bitplanes)
}

/// Everything about one component that does not change between resolutions.
#[derive(Clone, Copy, Debug)]
struct ComponentStyle<'a> {
    coding: &'a BlockCoding,
    quant: &'a Quant,
    depth: u8,
    levels: u8,
}

/// A rectangle on one grid: the tile on the reference grid, or a tile
/// component on its own.
#[derive(Clone, Copy, Debug)]
pub struct TileRectangle {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

/// One resolution level: its rectangle, its precinct grid, and its subbands.
fn build_resolution(
    tile: TileRectangle,
    style: ComponentStyle<'_>,
    resolution: usize,
    budget: &mut usize,
) -> Option<Resolution> {
    let ComponentStyle {
        coding,
        quant,
        depth,
        levels,
    } = style;
    let shift = u32::from(levels) - u32::try_from(resolution).ok()?;
    let rx0 = ceil_shift(tile.x0, shift);
    let rx1 = ceil_shift(tile.x1, shift);
    let ry0 = ceil_shift(tile.y0, shift);
    let ry1 = ceil_shift(tile.y1, shift);
    let (ppx, ppy) = coding
        .precincts
        .get(resolution)
        .copied()
        .unwrap_or((15, 15));

    let precincts_wide = if rx1 > rx0 {
        ceil_shift(rx1, u32::from(ppx)) - (rx0 >> ppx)
    } else {
        0
    };
    let precincts_high = if ry1 > ry0 {
        ceil_shift(ry1, u32::from(ppy)) - (ry0 >> ppy)
    } else {
        0
    };

    // B.7: inside a subband the precinct and code-block partitions are on the
    // band's own grid, which is half the resolution's above level 0.
    // The marker reader refuses a zero exponent above resolution zero, so this
    // cannot go below zero; it saturates rather than trusting that from here.
    let band_ppx = u32::from(ppx).saturating_sub(u32::from(resolution > 0));
    let band_ppy = u32::from(ppy).saturating_sub(u32::from(resolution > 0));
    let block_width = u32::from(coding.block_width).min(band_ppx);
    let block_height = u32::from(coding.block_height).min(band_ppy);

    // Resolution zero is the one low-pass band left at the bottom of the
    // decomposition; every level above it adds the three detail bands that
    // were taken off at that step.
    let kinds: &[(BandKind, u32, u32, u32)] = if resolution == 0 {
        &[(BandKind::LowLow, 0, 0, 0)]
    } else {
        &[
            (BandKind::HighLow, 1, 0, 1),
            (BandKind::LowHigh, 0, 1, 1),
            (BandKind::HighHigh, 1, 1, 2),
        ]
    };
    let decomposition = if resolution == 0 {
        u32::from(levels)
    } else {
        u32::from(levels) - u32::try_from(resolution).ok()? + 1
    };

    let mut bands = Vec::with_capacity(kinds.len());
    for (index, &(kind, xob, yob, gain)) in kinds.iter().enumerate() {
        let (bx0, bx1, by0, by1) = if resolution == 0 {
            (rx0, rx1, ry0, ry1)
        } else {
            // B.5: a detail band's rectangle is the tile component's, moved
            // half a sample in the direction the band is high-pass in, and
            // then rounded down one more level than the resolution.
            let offset = 1u32 << (decomposition - 1);
            (
                ceil_shift(tile.x0.saturating_sub(offset * xob), decomposition),
                ceil_shift(tile.x1.saturating_sub(offset * xob), decomposition),
                ceil_shift(tile.y0.saturating_sub(offset * yob), decomposition),
                ceil_shift(tile.y1.saturating_sub(offset * yob), decomposition),
            )
        };
        let (step, range, bitplanes) = band_step(quant, levels, resolution, index, depth, gain);
        let count =
            (bx1.saturating_sub(bx0) as usize).checked_mul(by1.saturating_sub(by0) as usize)?;
        *budget = budget.checked_sub(count)?;
        let mut band = Band {
            kind,
            x0: bx0,
            y0: by0,
            x1: bx1,
            y1: by1,
            step,
            range,
            bitplanes,
            coefficients: vec![0; count],
            precincts: Vec::new(),
        };
        band.precincts = build_precincts(
            &band,
            rx0,
            ry0,
            precincts_wide,
            precincts_high,
            band_ppx,
            band_ppy,
            resolution > 0,
            block_width,
            block_height,
        );
        bands.push(band);
    }

    Some(Resolution {
        x0: rx0,
        y0: ry0,
        x1: rx1,
        y1: ry1,
        ppx,
        ppy,
        precincts_wide,
        precincts_high,
        bands,
    })
}

/// Builds one tile component's whole tree of rectangles.
pub fn build_component(
    image: &ImageHeader,
    style: &TileStyle,
    component: usize,
    tile: TileRectangle,
    limits: Limits,
) -> Option<TileComponent> {
    let spec = image.components.get(component)?;
    let coding: &BlockCoding = style.coding.for_component(component);
    let quant = style.quant.get(component)?;
    let levels = coding.levels;

    let TileRectangle {
        x0: tile_x0,
        y0: tile_y0,
        x1: tile_x1,
        y1: tile_y1,
    } = tile;
    let x0 = tile_x0.div_ceil(spec.dx);
    let x1 = tile_x1.div_ceil(spec.dx);
    let y0 = tile_y0.div_ceil(spec.dy);
    let y1 = tile_y1.div_ceil(spec.dy);
    let mut budget = limits.max_tile_coefficients;

    let mut resolutions = Vec::with_capacity(usize::from(levels) + 1);
    for resolution in 0..=usize::from(levels) {
        resolutions.push(build_resolution(
            TileRectangle { x0, y0, x1, y1 },
            ComponentStyle {
                coding,
                quant,
                depth: spec.depth,
                levels,
            },
            resolution,
            &mut budget,
        )?);
    }

    Some(TileComponent {
        x0,
        y0,
        x1,
        y1,
        levels,
        resolutions,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_precincts(
    band: &Band,
    resolution_x0: u32,
    resolution_y0: u32,
    precincts_wide: u32,
    precincts_high: u32,
    band_ppx: u32,
    band_ppy: u32,
    halved: bool,
    block_width: u32,
    block_height: u32,
) -> Vec<Precinct> {
    let mut precincts = Vec::new();
    if precincts_wide == 0 || precincts_high == 0 {
        return precincts;
    }
    // The precinct grid is anchored at zero on the *resolution*, so on the
    // band it is anchored at zero too, one level down.
    // B.7: the precinct grid is anchored at zero on the resolution, so the same
    // grid one level down is where the band's precincts start.
    let first_x = resolution_x0 >> (band_ppx + u32::from(halved));
    let first_y = resolution_y0 >> (band_ppy + u32::from(halved));
    for row in 0..precincts_high {
        for column in 0..precincts_wide {
            let px0 = (first_x + column) << band_ppx;
            let py0 = (first_y + row) << band_ppy;
            let px1 = px0 + (1 << band_ppx);
            let py1 = py0 + (1 << band_ppy);
            let cx0 = px0.max(band.x0);
            let cy0 = py0.max(band.y0);
            let cx1 = px1.min(band.x1);
            let cy1 = py1.min(band.y1);
            let (blocks_wide, blocks_high, blocks) = if cx1 <= cx0 || cy1 <= cy0 {
                (0, 0, Vec::new())
            } else {
                let first_block_x = cx0 >> block_width;
                let first_block_y = cy0 >> block_height;
                let last_block_x = (cx1 - 1) >> block_width;
                let last_block_y = (cy1 - 1) >> block_height;
                let wide = last_block_x - first_block_x + 1;
                let high = last_block_y - first_block_y + 1;
                let mut blocks = Vec::with_capacity((wide * high) as usize);
                for by in 0..high {
                    for bx in 0..wide {
                        let bx0 = ((first_block_x + bx) << block_width).max(cx0);
                        let by0 = ((first_block_y + by) << block_height).max(cy0);
                        let bx1 = ((first_block_x + bx + 1) << block_width).min(cx1);
                        let by1 = ((first_block_y + by + 1) << block_height).min(cy1);
                        blocks.push(CodeBlock {
                            x0: bx0,
                            y0: by0,
                            x1: bx1,
                            y1: by1,
                            included: false,
                            lblock: 3,
                            zero_bitplanes: 0,
                            passes: 0,
                            data: Vec::new(),
                            segments: Vec::new(),
                        });
                    }
                }
                (wide, high, blocks)
            };
            precincts.push(Precinct {
                blocks_wide,
                blocks,
                inclusion: TagTree::new(blocks_wide, blocks_high),
                missing_bitplanes: TagTree::new(blocks_wide, blocks_high),
            });
        }
    }
    precincts
}

/// One packet's address: which layer, resolution, component and precinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketAddress {
    pub layer: u16,
    pub resolution: usize,
    pub component: usize,
    pub precinct: u32,
}

/// Every packet of a tile, in the order the codestream writes them. B.12.
///
/// The three position-driven progressions are generated by listing every
/// precinct with the reference-grid point it starts at and sorting on the keys
/// that progression orders by. That is what the standard's nested loops
/// compute; writing it as a sort makes the ordering itself readable.
pub fn packet_order(
    components: &[TileComponent],
    image: &ImageHeader,
    coding: &Coding,
) -> Vec<PacketAddress> {
    let layers = coding.layers;
    let max_resolutions = components
        .iter()
        .map(|component| component.resolutions.len())
        .max()
        .unwrap_or(0);
    let mut order = Vec::new();
    match coding.progression {
        Progression::LayerResolutionComponentPosition => {
            for layer in 0..layers {
                for resolution in 0..max_resolutions {
                    for (component, tile) in components.iter().enumerate() {
                        let Some(level) = tile.resolutions.get(resolution) else {
                            continue;
                        };
                        for precinct in 0..level.precinct_count() {
                            order.push(PacketAddress {
                                layer,
                                resolution,
                                component,
                                precinct,
                            });
                        }
                    }
                }
            }
        }
        Progression::ResolutionLayerComponentPosition => {
            for resolution in 0..max_resolutions {
                for layer in 0..layers {
                    for (component, tile) in components.iter().enumerate() {
                        let Some(level) = tile.resolutions.get(resolution) else {
                            continue;
                        };
                        for precinct in 0..level.precinct_count() {
                            order.push(PacketAddress {
                                layer,
                                resolution,
                                component,
                                precinct,
                            });
                        }
                    }
                }
            }
        }
        progression => {
            let mut placed = positions(components, image);
            match progression {
                Progression::ResolutionPositionComponentLayer => {
                    placed.sort_by_key(|slot| (slot.resolution, slot.y, slot.x, slot.component));
                }
                Progression::PositionComponentResolutionLayer => {
                    placed.sort_by_key(|slot| (slot.y, slot.x, slot.component, slot.resolution));
                }
                _ => {
                    placed.sort_by_key(|slot| (slot.component, slot.y, slot.x, slot.resolution));
                }
            }
            for slot in placed {
                for layer in 0..layers {
                    order.push(PacketAddress {
                        layer,
                        resolution: slot.resolution,
                        component: slot.component,
                        precinct: slot.precinct,
                    });
                }
            }
        }
    }
    order
}

struct Slot {
    resolution: usize,
    component: usize,
    precinct: u32,
    x: u64,
    y: u64,
}

/// Where each precinct starts on the reference grid, which is the coordinate
/// the position-driven progressions sort by.
fn positions(components: &[TileComponent], image: &ImageHeader) -> Vec<Slot> {
    let mut slots = Vec::new();
    for (component, tile) in components.iter().enumerate() {
        let spec = &image.components[component];
        for (resolution, level) in tile.resolutions.iter().enumerate() {
            let shift = u32::from(tile.levels) - u32::try_from(resolution).unwrap_or(0);
            for index in 0..level.precinct_count() {
                let column = index % level.precincts_wide.max(1);
                let row = index / level.precincts_wide.max(1);
                let x = u64::from((level.x0 >> level.ppx) + column) << level.ppx;
                let y = u64::from((level.y0 >> level.ppy) + row) << level.ppy;
                slots.push(Slot {
                    resolution,
                    component,
                    precinct: index,
                    x: (x << shift) * u64::from(spec.dx),
                    y: (y << shift) * u64::from(spec.dy),
                });
            }
        }
    }
    slots
}

/// Whether the two bytes at `offset` are the given marker.
fn marker_is(data: &[u8], offset: usize, marker: u16) -> bool {
    data.get(offset..offset + 2)
        .is_some_and(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]) == marker)
}

/// How many coding passes a packet header just signalled, B.10.6.
fn number_of_passes(reader: &mut BitReader) -> u32 {
    if reader.bit() == 0 {
        return 1;
    }
    if reader.bit() == 0 {
        return 2;
    }
    let value = reader.bits(2);
    if value < 3 {
        return 3 + value;
    }
    let value = reader.bits(5);
    if value < 31 {
        return 6 + value;
    }
    37 + reader.bits(7)
}

/// Reads every packet of one tile out of its concatenated tile-part data.
///
/// Returns how many bytes were consumed, and whether the packets ran past the
/// end of what the tile-parts held.
pub fn read_packets(
    data: &[u8],
    components: &mut [TileComponent],
    image: &ImageHeader,
    style: &TileStyle,
) -> bool {
    let order = packet_order(components, image, &style.coding);
    let mut offset = 0usize;
    let mut overran = false;
    for address in order {
        if offset >= data.len() {
            overran = offset > data.len();
            break;
        }
        // B.9: a start-of-packet marker may precede any packet, carrying a
        // sequence number this decoder does not need.
        if style.coding.start_of_packet && marker_is(data, offset, codestream::SOP) {
            // B.9: two bytes of marker, two of length, two of sequence number.
            offset += 6;
        }
        let Some(component) = components.get_mut(address.component) else {
            continue;
        };
        let block_style = style.coding.for_component(address.component).style;
        let Some(resolution) = component.resolutions.get_mut(address.resolution) else {
            continue;
        };
        let Some(consumed) = read_packet(
            &data[offset.min(data.len())..],
            resolution,
            address,
            block_style,
            style.coding.end_of_packet_header,
        ) else {
            overran = true;
            break;
        };
        offset += consumed;
        if offset > data.len() {
            overran = true;
            break;
        }
    }
    overran
}

/// What one packet header said about one code-block, before its bytes are read.
///
/// The header and the body of a packet are separate passes over the same
/// addresses, because the header must be read to its end -- and aligned -- to
/// know where the bodies start.
struct Contribution {
    band: usize,
    precinct: usize,
    block: usize,
    /// One `(passes, byte length)` pair per codeword segment.
    lengths: Vec<(u32, usize)>,
}

/// One packet: its header, then its code-block bytes. Returns bytes consumed.
fn read_packet(
    data: &[u8],
    resolution: &mut Resolution,
    address: PacketAddress,
    block_style: u8,
    end_of_packet_header: bool,
) -> Option<usize> {
    let mut reader = BitReader::new(data);
    let mut contributions: Vec<Contribution> = Vec::new();
    if reader.bit() == 1 {
        for (band_index, band) in resolution.bands.iter_mut().enumerate() {
            if band.is_empty() {
                continue;
            }
            let Some(precinct) = band.precincts.get_mut(address.precinct as usize) else {
                continue;
            };
            for index in 0..precinct.blocks.len() {
                let x = u32::try_from(index).ok()? % precinct.blocks_wide.max(1);
                let y = u32::try_from(index).ok()? / precinct.blocks_wide.max(1);
                let block = &precinct.blocks[index];
                let already = block.included;
                let included = if already {
                    reader.bit() == 1
                } else {
                    precinct
                        .inclusion
                        .decode(&mut reader, x, y, u32::from(address.layer) + 1)
                        .is_some()
                };
                if !included {
                    continue;
                }
                if !already {
                    let zero = precinct.missing_bitplanes.decode_fully(&mut reader, x, y);
                    let block = &mut precinct.blocks[index];
                    block.zero_bitplanes = zero;
                    block.included = true;
                    block.lblock = 3;
                }
                let passes = number_of_passes(&mut reader);
                let block = &mut precinct.blocks[index];
                while reader.bit() == 1 {
                    block.lblock += 1;
                }
                // B.10.7: the contribution is split wherever the code-block
                // style says a codeword segment ends, and each piece carries
                // its own length.
                let first_pass = block.passes;
                let mut lengths = Vec::new();
                let mut remaining = passes;
                let mut pass = first_pass;
                while remaining > 0 {
                    let mut in_segment = 1;
                    while !t1::terminates_after(block_style, pass + in_segment - 1)
                        && in_segment < remaining
                    {
                        in_segment += 1;
                    }
                    let extra = in_segment.ilog2();
                    let length = reader.bits(block.lblock + extra) as usize;
                    lengths.push((in_segment, length));
                    pass += in_segment;
                    remaining -= in_segment;
                }
                block.passes += passes;
                contributions.push(Contribution {
                    band: band_index,
                    precinct: address.precinct as usize,
                    block: index,
                    lengths,
                });
            }
        }
    }
    reader.align();
    let mut offset = reader.consumed();
    if end_of_packet_header && marker_is(data, offset, codestream::EPH) {
        offset += 2;
    }
    if offset > data.len() {
        return None;
    }

    for contribution in contributions {
        let band = resolution.bands.get_mut(contribution.band)?;
        let precinct = band.precincts.get_mut(contribution.precinct)?;
        let block = precinct.blocks.get_mut(contribution.block)?;
        for (passes, length) in contribution.lengths {
            let end = offset.checked_add(length)?;
            if end > data.len() {
                return None;
            }
            let start = block.data.len();
            block.data.extend_from_slice(&data[offset..end]);
            block.segments.push(Segment {
                start,
                length,
                passes,
            });
            offset = end;
        }
    }
    Some(offset)
}

/// Decodes every code-block of a tile component and writes the coefficients
/// into their bands. Returns whether any block reported itself damaged.
pub fn decode_blocks(component: &mut TileComponent, block_style: u8, roi_shift: u8) -> bool {
    let mut damaged = false;
    for resolution in &mut component.resolutions {
        for band in &mut resolution.bands {
            if band.is_empty() {
                continue;
            }
            let band_width = band.width() as usize;
            let kind = band.kind;
            let bitplanes = band.bitplanes;
            let (x0, y0) = (band.x0, band.y0);
            for precinct in &mut band.precincts {
                for block in &mut precinct.blocks {
                    if !block.included || block.passes == 0 {
                        continue;
                    }
                    // Segments are merged where no termination falls between
                    // them, because the arithmetic coder runs straight through
                    // a packet boundary that did not terminate it.
                    let segments = merge_segments(&block.segments, block_style);
                    let (values, corrupt) = t1::decode_block(
                        &block.data,
                        &segments,
                        t1::BlockShape {
                            width: block.x1 - block.x0,
                            height: block.y1 - block.y0,
                            band: kind,
                            zero_bitplanes: block.zero_bitplanes,
                            bitplanes,
                            style: block_style,
                        },
                    );
                    damaged |= corrupt;
                    let width = (block.x1 - block.x0) as usize;
                    for (index, value) in values.iter().enumerate() {
                        if *value == 0 {
                            continue;
                        }
                        let row = (block.y0 - y0) as usize + index / width;
                        let column = (block.x0 - x0) as usize + index % width;
                        let mut value = *value;
                        // E.2: the encoder shifted the region of interest up so
                        // that it would be coded first. A coefficient at or
                        // above the shift is one of those, and is shifted back
                        // down; one below it was never scaled. The shift is
                        // clamped because the marker can declare more than the
                        // register has.
                        let shift = u32::from(roi_shift).min(31);
                        if shift > 0 && value.unsigned_abs() >= 1u32 << shift {
                            value >>= shift;
                        }
                        band.coefficients[row * band_width + column] = value;
                    }
                }
            }
        }
    }
    damaged
}

/// Joins consecutive contributions that no termination separates.
fn merge_segments(segments: &[Segment], style: u8) -> Vec<Segment> {
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    let mut pass = 0u32;
    for segment in segments {
        let continues = merged.last().is_some_and(|last| {
            !t1::terminates_after(style, pass - 1) && last.start + last.length == segment.start
        });
        if continues && pass > 0 {
            if let Some(last) = merged.last_mut() {
                last.length += segment.length;
                last.passes += segment.passes;
            }
        } else {
            merged.push(*segment);
        }
        pass += segment.passes;
    }
    merged
}
