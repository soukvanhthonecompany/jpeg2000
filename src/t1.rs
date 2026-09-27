//! EBCOT tier-1: one code-block of wavelet coefficients, decoded bit-plane by
//! bit-plane. ISO/IEC 15444-1 Annex D.
//!
//! A code-block is coded as a sequence of *passes*, three per bit-plane after
//! the first: significance propagation, magnitude refinement, and cleanup. The
//! first pass of all is a cleanup pass on the most significant bit-plane that
//! is not known to be empty. Each pass visits the block in stripes four rows
//! tall, and what a pass decides about a coefficient depends on which of its
//! eight neighbours are already significant -- which is why the whole thing is
//! written as one walk over a flags array with a one-cell border, rather than
//! as three independent loops with bounds checks.

use crate::mq::{Context, MqDecoder, RawDecoder};

/// Which of the four subbands a code-block sits in. The neighbourhood that
/// decides a significance context is read differently in each of three cases,
/// because the wavelet has already told us which direction the band's energy
/// runs in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BandKind {
    LowLow,
    HighLow,
    LowHigh,
    HighHigh,
}

/// Code-block style bits from the coding-style marker segment, D.6.
///
/// Bit `0x10`, predictable termination, is not here: it is a promise the
/// encoder makes about where its codeword segments end, and a decoder that
/// does not check the promise has nothing to do differently.
pub mod style {
    /// Raw bits instead of the arithmetic coder, from the fifth bit-plane on.
    pub const BYPASS: u8 = 0x01;
    /// Contexts return to their initial states at every pass boundary.
    pub const RESET_CONTEXTS: u8 = 0x02;
    /// Every pass is its own codeword segment.
    pub const TERMINATE_ALL: u8 = 0x04;
    /// A stripe may not look at the stripe below it.
    pub const VERTICALLY_CAUSAL: u8 = 0x08;
    /// Each cleanup pass ends with the four bits 1010.
    pub const SEGMENTATION_SYMBOLS: u8 = 0x20;
}

const SIGNIFICANT: u8 = 1;
const VISITED: u8 = 2;
const REFINED: u8 = 4;
const NEGATIVE: u8 = 8;

/// A coefficient is carried as an `i32`. A codestream may declare more
/// bit-planes than that can hold -- seven guard bits and a large exponent reach
/// thirty-seven -- and no real one does, so the extra planes are dropped rather
/// than shifted off the top of the register.
const MOST_PLANES: u32 = 30;

const RUN_LENGTH: usize = 17;
const UNIFORM: usize = 18;

/// One codeword segment: its bytes, and how many passes were coded into it.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub start: usize,
    pub length: usize,
    pub passes: u32,
}

/// Whether a codeword segment ends after the pass with this index.
///
/// B.10.7.2. The answer is the same for the encoder that wrote the packet
/// header and the decoder that reads it, so both ask here: the number of
/// lengths a packet header carries is the number of times this says yes.
#[must_use]
pub const fn terminates_after(style: u8, pass: u32) -> bool {
    if style & self::style::TERMINATE_ALL != 0 {
        return true;
    }
    if style & self::style::BYPASS != 0 {
        if pass < 9 {
            return false;
        }
        if pass == 9 {
            return true;
        }
        // From the tenth pass on: the two raw passes of a bit-plane are one
        // segment and its cleanup pass is another.
        return !(pass - 10).is_multiple_of(3);
    }
    false
}

/// Whether the pass with this index is coded raw rather than arithmetically.
const fn is_raw(style: u8, pass: u32) -> bool {
    style & self::style::BYPASS != 0 && pass >= 10 && (pass - 10) % 3 != 2
}

/// What kind of pass the index names: cleanup first, then the repeating three.
const fn pass_kind(pass: u32) -> Pass {
    if pass == 0 {
        Pass::Cleanup
    } else {
        match (pass - 1) % 3 {
            0 => Pass::Significance,
            1 => Pass::Refinement,
            _ => Pass::Cleanup,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pass {
    Significance,
    Refinement,
    Cleanup,
}

/// A code-block being decoded.
struct Block<'a> {
    width: usize,
    height: usize,
    stride: usize,
    flags: Vec<u8>,
    magnitude: Vec<i32>,
    band: BandKind,
    style: u8,
    contexts: [Context; 19],
    mq: MqDecoder<'a>,
    raw: RawDecoder<'a>,
    raw_mode: bool,
    /// Set when a segmentation symbol did not read 1010, which means the
    /// codeword segment was damaged somewhere before it.
    pub corrupt: bool,
}

fn initial_contexts() -> [Context; 19] {
    let mut contexts = [Context::default(); 19];
    contexts[0] = Context::new(4, 0);
    contexts[RUN_LENGTH] = Context::new(3, 0);
    contexts[UNIFORM] = Context::new(46, 0);
    contexts
}

/// What a code-block is, apart from its bits.
#[derive(Clone, Copy, Debug)]
pub struct BlockShape {
    pub width: u32,
    pub height: u32,
    pub band: BandKind,
    /// How many of the block's most significant bit-planes the packet header
    /// declared empty.
    pub zero_bitplanes: u32,
    /// How many bit-planes the band's coefficients have in total.
    pub bitplanes: u32,
    pub style: u8,
}

/// Decodes one code-block into signed coefficients, row-major.
///
/// `data` holds every codeword segment the packets contributed, concatenated,
/// and `segments` says where the terminations fall.
///
/// Returns the coefficients and whether a segmentation symbol reported damage.
#[must_use]
pub fn decode_block(data: &[u8], segments: &[Segment], shape: BlockShape) -> (Vec<i32>, bool) {
    let BlockShape {
        band,
        zero_bitplanes,
        bitplanes,
        style,
        ..
    } = shape;
    let width = shape.width as usize;
    let height = shape.height as usize;
    let count = width * height;
    if count == 0 || bitplanes <= zero_bitplanes {
        return (vec![0; count], false);
    }
    let coded_planes = (bitplanes - zero_bitplanes).min(MOST_PLANES);

    let stride = width + 2;
    let mut block = Block {
        width,
        height,
        stride,
        flags: vec![0; stride * (height + 2)],
        magnitude: vec![0i32; count],
        band,
        style,
        contexts: initial_contexts(),
        mq: MqDecoder::new(&[]),
        raw: RawDecoder::new(&[]),
        raw_mode: false,
        corrupt: false,
    };

    let mut pass = 0u32;
    let mut plane = coded_planes - 1;
    let mut segment_index = 0usize;
    let mut passes_left_in_segment = 0u32;
    loop {
        if passes_left_in_segment == 0 {
            let Some(segment) = segments.get(segment_index) else {
                break;
            };
            let end = segment.start.saturating_add(segment.length).min(data.len());
            let bytes = data.get(segment.start..end).unwrap_or(&[]);
            block.raw_mode = is_raw(style, pass);
            if block.raw_mode {
                block.raw = RawDecoder::new(bytes);
            } else {
                block.mq = MqDecoder::new(bytes);
            }
            passes_left_in_segment = segment.passes;
            segment_index += 1;
            if passes_left_in_segment == 0 {
                continue;
            }
        } else {
            // Inside a segment the coder continues; only the raw/arithmetic
            // switch of the bypass style can happen here, and that style
            // always terminates at the switch, so there is nothing to do.
        }

        if style & self::style::RESET_CONTEXTS != 0 {
            block.contexts = initial_contexts();
        }
        match pass_kind(pass) {
            Pass::Significance => block.significance_pass(plane),
            Pass::Refinement => block.refinement_pass(plane),
            Pass::Cleanup => block.cleanup_pass(plane),
        }

        passes_left_in_segment -= 1;
        pass += 1;
        if pass_kind(pass - 1) == Pass::Cleanup {
            if plane == 0 {
                break;
            }
            plane -= 1;
        }
    }

    // Magnitudes are kept where the band's own bit-planes put them, so a caller
    // dequantising or inverting the wavelet reads a coefficient at its true
    // scale without knowing how many planes were skipped.
    let out = block
        .magnitude
        .iter()
        .enumerate()
        .map(|(index, magnitude)| {
            let (row, column) = (index / width, index % width);
            if *magnitude != 0 && block.flags[(row + 1) * stride + column + 1] & NEGATIVE != 0 {
                -*magnitude
            } else {
                *magnitude
            }
        })
        .collect();
    (out, block.corrupt)
}

impl Block<'_> {
    const fn at(&self, x: usize, y: usize) -> usize {
        (y + 1) * self.stride + (x + 1)
    }

    fn bit(&mut self, context: usize) -> u8 {
        if self.raw_mode {
            self.raw.decode()
        } else {
            let mut state = self.contexts[context];
            let bit = self.mq.decode(&mut state);
            self.contexts[context] = state;
            bit
        }
    }

    fn uniform_bit(&mut self) -> u8 {
        if self.raw_mode {
            self.raw.decode()
        } else {
            let mut state = self.contexts[UNIFORM];
            let bit = self.mq.decode(&mut state);
            self.contexts[UNIFORM] = state;
            bit
        }
    }

    /// The horizontal, vertical and diagonal neighbour counts, in the border
    /// coordinates where a missing neighbour is simply an all-zero cell.
    fn neighbourhood(&self, x: usize, y: usize) -> (u32, u32, u32) {
        let index = self.at(x, y);
        let stride = self.stride;
        let causal = self.style & self::style::VERTICALLY_CAUSAL != 0;
        let below_visible = !causal || (y % 4) != 3;
        let bit = |offset: usize, enabled: bool| -> u32 {
            u32::from(enabled && self.flags[offset] & SIGNIFICANT != 0)
        };
        let horizontal = bit(index - 1, true) + bit(index + 1, true);
        let vertical = bit(index - stride, true) + bit(index + stride, below_visible);
        let diagonal = bit(index - stride - 1, true)
            + bit(index - stride + 1, true)
            + bit(index + stride - 1, below_visible)
            + bit(index + stride + 1, below_visible);
        (horizontal, vertical, diagonal)
    }

    /// Table D.1: the significance context of a coefficient, per band.
    fn significance_context(&self, x: usize, y: usize) -> usize {
        let (mut horizontal, mut vertical, diagonal) = self.neighbourhood(x, y);
        match self.band {
            BandKind::HighLow => std::mem::swap(&mut horizontal, &mut vertical),
            BandKind::HighHigh => {
                let sum = horizontal + vertical;
                return match (diagonal, sum) {
                    (0, 0) => 0,
                    (0, 1) => 1,
                    (0, _) => 2,
                    (1, 0) => 3,
                    (1, 1) => 4,
                    (1, _) => 5,
                    (2, 0) => 6,
                    (2, _) => 7,
                    _ => 8,
                };
            }
            BandKind::LowLow | BandKind::LowHigh => {}
        }
        match (horizontal, vertical, diagonal) {
            (1, 0, 0) => 5,
            (1, 0, _) => 6,
            (1, _, _) => 7,
            (0, 2, _) => 4,
            (0, 1, _) => 3,
            (0, 0, 0) => 0,
            (0, 0, 1) => 1,
            (0, 0, _) => 2,
            // Two significant horizontal neighbours, which is as sure as this
            // table gets, and anything a malformed count could produce.
            _ => 8,
        }
    }

    /// Table D.3: the sign context, and whether the decoded bit is inverted.
    ///
    /// Each of the four orthogonal neighbours contributes the sign it carries,
    /// or nothing when it is not significant; the two axes are then clamped,
    /// because what the context expresses is agreement, not magnitude.
    fn sign_context(&self, x: usize, y: usize) -> (usize, u8) {
        let index = self.at(x, y);
        let stride = self.stride;
        let causal = self.style & self::style::VERTICALLY_CAUSAL != 0;
        let below_visible = !causal || (y % 4) != 3;
        let contribution = |offset: usize, enabled: bool| -> i32 {
            let flags = self.flags[offset];
            if !enabled || flags & SIGNIFICANT == 0 {
                0
            } else if flags & NEGATIVE == 0 {
                1
            } else {
                -1
            }
        };
        let horizontal =
            (contribution(index - 1, true) + contribution(index + 1, true)).clamp(-1, 1);
        let vertical = (contribution(index - stride, true)
            + contribution(index + stride, below_visible))
        .clamp(-1, 1);
        match (horizontal, vertical) {
            (1, 1) => (13, 0),
            (1, 0) => (12, 0),
            (1, -1) => (11, 0),
            (0, 1) => (10, 0),
            (0, 0) => (9, 0),
            (0, -1) => (10, 1),
            (-1, 1) => (11, 1),
            (-1, 0) => (12, 1),
            _ => (13, 1),
        }
    }

    /// Decodes the sign of a coefficient that has just become significant.
    fn decode_sign(&mut self, x: usize, y: usize) {
        let (context, invert) = self.sign_context(x, y);
        let bit = if self.raw_mode {
            self.raw.decode()
        } else {
            self.bit(context)
        };
        if bit ^ invert == 1 {
            let index = self.at(x, y);
            self.flags[index] |= NEGATIVE;
        }
    }

    /// A coefficient that has just been found significant at `plane`.
    ///
    /// The magnitude is set to the middle of what is still unknown, not to the
    /// bottom of it: everything below `plane` is undecoded, so the best
    /// estimate is the bit that was just decoded plus half of the next one. A
    /// block whose passes all arrive ends up exact anyway -- the final
    /// refinement at plane zero removes the half again -- and one truncated by
    /// a layer that was never sent ends up in the middle of its interval
    /// instead of at the low end of it.
    fn make_significant(&mut self, x: usize, y: usize, plane: u32) {
        let index = self.at(x, y);
        self.flags[index] |= SIGNIFICANT;
        let one = 1i32 << plane;
        self.magnitude[y * self.width + x] = one | (one >> 1);
        self.decode_sign(x, y);
    }

    /// D.3. Every coefficient with a significant neighbour, and only those.
    fn significance_pass(&mut self, plane: u32) {
        for stripe in (0..self.height).step_by(4) {
            for x in 0..self.width {
                for y in stripe..(stripe + 4).min(self.height) {
                    let index = self.at(x, y);
                    if self.flags[index] & SIGNIFICANT != 0 {
                        continue;
                    }
                    let context = self.significance_context(x, y);
                    if context == 0 {
                        continue;
                    }
                    let bit = if self.raw_mode {
                        self.raw.decode()
                    } else {
                        self.bit(context)
                    };
                    if bit == 1 {
                        self.make_significant(x, y, plane);
                    }
                    self.flags[index] |= VISITED;
                }
            }
        }
    }

    /// D.5. Every coefficient already significant that this plane did not just
    /// make so.
    fn refinement_pass(&mut self, plane: u32) {
        for stripe in (0..self.height).step_by(4) {
            for x in 0..self.width {
                for y in stripe..(stripe + 4).min(self.height) {
                    let index = self.at(x, y);
                    let flags = self.flags[index];
                    if flags & SIGNIFICANT == 0 || flags & VISITED != 0 {
                        continue;
                    }
                    let context = if flags & REFINED != 0 {
                        16
                    } else {
                        let (horizontal, vertical, diagonal) = self.neighbourhood(x, y);
                        if horizontal + vertical + diagonal > 0 {
                            15
                        } else {
                            14
                        }
                    };
                    let bit = if self.raw_mode {
                        self.raw.decode()
                    } else {
                        self.bit(context)
                    };
                    // The estimate carried half of this plane already, so a
                    // one moves it up by half a plane and a zero moves it down.
                    let half = (1i32 << plane) >> 1;
                    let magnitude = &mut self.magnitude[y * self.width + x];
                    if bit == 1 {
                        *magnitude += half;
                    } else {
                        *magnitude -= if plane > 0 { half } else { 1 };
                    }
                    self.flags[index] |= REFINED;
                }
            }
        }
    }

    /// D.4. Everything the other two passes did not touch, with the run-length
    /// shortcut for a column of four that is entirely quiet.
    fn cleanup_pass(&mut self, plane: u32) {
        for stripe in (0..self.height).step_by(4) {
            let rows = (stripe + 4).min(self.height) - stripe;
            for x in 0..self.width {
                let mut y = stripe;
                if rows == 4 && self.column_is_quiet(x, stripe) {
                    if self.bit(RUN_LENGTH) == 0 {
                        continue;
                    }
                    let first =
                        usize::from(self.uniform_bit()) * 2 + usize::from(self.uniform_bit());
                    y = stripe + first;
                    self.make_significant(x, y, plane);
                    y += 1;
                }
                while y < stripe + rows {
                    let index = self.at(x, y);
                    let flags = self.flags[index];
                    if flags & (SIGNIFICANT | VISITED) == 0 {
                        let context = self.significance_context(x, y);
                        if self.bit(context) == 1 {
                            self.make_significant(x, y, plane);
                        }
                    }
                    y += 1;
                }
            }
            // The visited marks belong to one bit-plane only.
            for x in 0..self.width {
                for y in stripe..stripe + rows {
                    let index = self.at(x, y);
                    self.flags[index] &= !VISITED;
                }
            }
        }
        if self.style & self::style::SEGMENTATION_SYMBOLS != 0 {
            let mut symbol = 0u8;
            for _ in 0..4 {
                symbol = (symbol << 1) | self.uniform_bit();
            }
            if symbol != 0b1010 {
                self.corrupt = true;
            }
        }
    }

    /// Whether all four coefficients of a stripe column are insignificant, not
    /// yet visited, and have no significant neighbour at all.
    fn column_is_quiet(&self, x: usize, stripe: usize) -> bool {
        (0..4).all(|offset| {
            let y = stripe + offset;
            let index = self.at(x, y);
            self.flags[index] & (SIGNIFICANT | VISITED) == 0 && self.significance_context(x, y) == 0
        })
    }
}
