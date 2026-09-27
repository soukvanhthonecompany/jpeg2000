//! The MQ arithmetic decoder of ISO/IEC 15444-1 Annex C.
//!
//! The same coder appears in JBIG2 as the MQ-coder and here as the entropy
//! stage of EBCOT. It is written out in the standard as flowcharts over a
//! handful of registers, and this is those flowcharts: `A` the interval, `C`
//! the code register, `CT` the bits left in the byte being consumed, and one
//! state index plus one "more probable symbol" bit per context.
//!
//! Nothing here reads past the end of its input. The standard's own answer to
//! running out of data is to feed `0xFF` forever, which is what a terminated
//! codeword segment is padded with anyway, so a truncated segment decodes to
//! whatever the coder would have produced -- it does not fail.

/// Qe, the next state on an MPS, the next state on an LPS, and whether an LPS
/// exchanges the meaning of the context's MPS bit. Table C.2 of the standard,
/// transcribed once.
const QE: [(u16, u8, u8, bool); 47] = [
    (0x5601, 1, 1, true),
    (0x3401, 2, 6, false),
    (0x1801, 3, 9, false),
    (0x0AC1, 4, 12, false),
    (0x0521, 5, 29, false),
    (0x0221, 38, 33, false),
    (0x5601, 7, 6, true),
    (0x5401, 8, 14, false),
    (0x4801, 9, 14, false),
    (0x3801, 10, 14, false),
    (0x3001, 11, 17, false),
    (0x2401, 12, 18, false),
    (0x1C01, 13, 20, false),
    (0x1601, 29, 21, false),
    (0x5601, 15, 14, true),
    (0x5401, 16, 14, false),
    (0x5101, 17, 15, false),
    (0x4801, 18, 16, false),
    (0x3801, 19, 17, false),
    (0x3401, 20, 18, false),
    (0x3001, 21, 19, false),
    (0x2801, 22, 19, false),
    (0x2401, 23, 20, false),
    (0x2201, 24, 21, false),
    (0x1C01, 25, 22, false),
    (0x1801, 26, 23, false),
    (0x1601, 27, 24, false),
    (0x1401, 28, 25, false),
    (0x1201, 29, 26, false),
    (0x1101, 30, 27, false),
    (0x0AC1, 31, 28, false),
    (0x09C1, 32, 29, false),
    (0x08A1, 33, 30, false),
    (0x0521, 34, 31, false),
    (0x0441, 35, 32, false),
    (0x02A1, 36, 33, false),
    (0x0221, 37, 34, false),
    (0x0141, 38, 35, false),
    (0x0111, 39, 36, false),
    (0x0085, 40, 37, false),
    (0x0049, 41, 38, false),
    (0x0025, 42, 39, false),
    (0x0015, 43, 40, false),
    (0x0009, 44, 41, false),
    (0x0005, 45, 42, false),
    (0x0001, 45, 43, false),
    (0x5601, 46, 46, false),
];

/// One context's adaptive state: where it sits in [`QE`] and which symbol it
/// currently believes is more likely.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Context {
    pub index: u8,
    pub mps: u8,
}

impl Context {
    pub const fn new(index: u8, mps: u8) -> Self {
        Self { index, mps }
    }
}

pub struct MqDecoder<'a> {
    data: &'a [u8],
    /// The index of the byte *after* the one currently in `C`. Allowed to run
    /// past the end of `data`, where [`Self::byte`] answers `0xFF`.
    position: usize,
    a: u32,
    c: u32,
    ct: u32,
}

impl<'a> MqDecoder<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        let mut decoder = Self {
            data,
            position: 0,
            a: 0,
            c: 0,
            ct: 0,
        };
        decoder.init();
        decoder
    }

    fn byte(&self, index: usize) -> u32 {
        // C.3.4: past the end of a segment the decoder behaves as though the
        // marker `0xFF` had been read, which is exactly how a terminated
        // segment ends anyway.
        self.data.get(index).copied().map_or(0xFF, u32::from)
    }

    fn init(&mut self) {
        self.c = self.byte(0) << 16;
        self.byte_in();
        self.c <<= 7;
        self.ct -= 7;
        self.a = 0x8000;
    }

    fn byte_in(&mut self) {
        if self.byte(self.position) == 0xFF {
            if self.byte(self.position + 1) > 0x8F {
                // A marker, or the end: stuff ones and stop consuming.
                self.c += 0xFF00;
                self.ct = 8;
            } else {
                self.position += 1;
                self.c += self.byte(self.position) << 9;
                self.ct = 7;
            }
        } else {
            self.position += 1;
            self.c += self.byte(self.position) << 8;
            self.ct = 8;
        }
    }

    fn renormalise(&mut self) {
        loop {
            if self.ct == 0 {
                self.byte_in();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.a & 0x8000 != 0 {
                break;
            }
        }
    }

    /// Decodes one binary symbol in the given context, adapting it.
    pub fn decode(&mut self, context: &mut Context) -> u8 {
        let (qe, nmps, nlps, switch) = QE[context.index as usize];
        let qe = u32::from(qe);
        self.a = self.a.wrapping_sub(qe);
        let decision;
        if (self.c >> 16) < qe {
            // LPS exchange: the smaller sub-interval was taken.
            if self.a < qe {
                decision = context.mps;
                context.index = nmps;
            } else {
                decision = 1 - context.mps;
                if switch {
                    context.mps = 1 - context.mps;
                }
                context.index = nlps;
            }
            self.a = qe;
            self.renormalise();
        } else {
            self.c -= qe << 16;
            if self.a & 0x8000 == 0 {
                // MPS exchange.
                if self.a < qe {
                    decision = 1 - context.mps;
                    if switch {
                        context.mps = 1 - context.mps;
                    }
                    context.index = nlps;
                } else {
                    decision = context.mps;
                    context.index = nmps;
                }
                self.renormalise();
            } else {
                decision = context.mps;
            }
        }
        decision
    }
}

/// The raw bit reader the "selective arithmetic coding bypass" style switches
/// to, from D.6 of the standard.
///
/// It is not the MQ coder with a different table: it is plain bits, most
/// significant first, with one stuffed zero after every `0xFF` so that a
/// marker can never be forged inside a codeword segment.
pub struct RawDecoder<'a> {
    data: &'a [u8],
    position: usize,
    current: u32,
    bits: u32,
}

impl<'a> RawDecoder<'a> {
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            current: 0,
            bits: 0,
        }
    }

    pub fn decode(&mut self) -> u8 {
        if self.bits == 0 {
            let previous = self.current;
            self.current = self
                .data
                .get(self.position)
                .copied()
                .map_or(0xFF, u32::from);
            self.position += 1;
            // The stuffed bit: a byte following `0xFF` carries only seven.
            self.bits = if previous == 0xFF { 7 } else { 8 };
        }
        self.bits -= 1;
        ((self.current >> self.bits) & 1) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::{Context, MqDecoder, QE, RawDecoder};

    /// The encoder of Annex C, written only so the decoder can be tested.
    ///
    /// It is a separate set of flowcharts from the decoder -- CODEMPS, CODELPS,
    /// BYTEOUT and FLUSH, none of which the decoder contains -- so a sequence
    /// that survives a trip through both was not preserved by one mistake made
    /// twice. That is the point: this file's calibration is a round trip, not
    /// a table of numbers copied from somewhere and asserted.
    struct MqEncoder {
        /// The output, with one sentinel byte at the front that FLUSH's
        /// carry-propagation may increment and that is dropped at the end.
        out: Vec<u8>,
        a: u32,
        c: u32,
        ct: u32,
    }

    impl MqEncoder {
        fn new() -> Self {
            Self {
                out: vec![0],
                a: 0x8000,
                c: 0,
                ct: 12,
            }
        }

        fn last(&mut self) -> &mut u8 {
            self.out.last_mut().expect("the sentinel is never popped")
        }

        fn byte_out(&mut self) {
            if *self.last() == 0xFF {
                self.out.push((self.c >> 20) as u8);
                self.c &= 0xF_FFFF;
                self.ct = 7;
            } else if self.c < 0x800_0000 {
                self.out.push((self.c >> 19) as u8);
                self.c &= 0x7_FFFF;
                self.ct = 8;
            } else {
                *self.last() += 1;
                if *self.last() == 0xFF {
                    self.c &= 0x7FF_FFFF;
                    self.out.push((self.c >> 20) as u8);
                    self.c &= 0xF_FFFF;
                    self.ct = 7;
                } else {
                    self.out.push((self.c >> 19) as u8);
                    self.c &= 0x7_FFFF;
                    self.ct = 8;
                }
            }
        }

        fn renormalise(&mut self) {
            loop {
                self.a <<= 1;
                self.c <<= 1;
                self.ct -= 1;
                if self.ct == 0 {
                    self.byte_out();
                }
                if self.a & 0x8000 != 0 {
                    break;
                }
            }
        }

        fn encode(&mut self, context: &mut Context, decision: u8) {
            let (qe, nmps, nlps, switch) = QE[context.index as usize];
            let qe = u32::from(qe);
            if decision == context.mps {
                self.a -= qe;
                if self.a & 0x8000 == 0 {
                    if self.a < qe {
                        self.a = qe;
                    } else {
                        self.c += qe;
                    }
                    context.index = nmps;
                    self.renormalise();
                } else {
                    self.c += qe;
                }
            } else {
                self.a -= qe;
                if self.a < qe {
                    self.c += qe;
                } else {
                    self.a = qe;
                }
                if switch {
                    context.mps = 1 - context.mps;
                }
                context.index = nlps;
                self.renormalise();
            }
        }

        fn finish(mut self) -> Vec<u8> {
            // SETBITS, then two byte-outs, then the 0xFF 0xAC terminator.
            let temp = self.c + self.a;
            self.c |= 0xFFFF;
            if self.c >= temp {
                self.c -= 0x8000;
            }
            self.c <<= self.ct;
            self.byte_out();
            self.c <<= self.ct;
            self.byte_out();
            if *self.last() != 0xFF {
                self.out.push(0xFF);
            }
            self.out.push(0xAC);
            self.out.remove(0);
            self.out
        }
    }

    /// A sequence that is not random enough to be a coin flip and not regular
    /// enough to be one state: the coder has to actually adapt.
    fn decisions(count: usize) -> Vec<u8> {
        let mut state = 0x1234_5678u32;
        (0..count)
            .map(|index| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                // Long runs of one symbol drive the state index up; the
                // occasional flip drives it back down through the LPS path.
                u8::from((state >> 24).is_multiple_of(8) || index.is_multiple_of(97))
            })
            .collect()
    }

    #[test]
    fn a_sequence_survives_a_trip_through_the_encoder_and_back() {
        let wanted = decisions(4096);
        let mut encoder = MqEncoder::new();
        let mut context = Context::default();
        for &decision in &wanted {
            encoder.encode(&mut context, decision);
        }
        let coded = encoder.finish();

        let mut decoder = MqDecoder::new(&coded);
        let mut context = Context::default();
        let got: Vec<u8> = (0..wanted.len())
            .map(|_| decoder.decode(&mut context))
            .collect();
        assert_eq!(got, wanted);
    }

    /// The negative control: the round trip above must be able to fail.
    ///
    /// One flipped byte in the middle of a codeword segment has to change what
    /// comes out, or the test above would pass against a decoder that ignored
    /// its input entirely.
    #[test]
    fn one_damaged_byte_changes_what_comes_out() {
        let wanted = decisions(4096);
        let mut encoder = MqEncoder::new();
        let mut context = Context::default();
        for &decision in &wanted {
            encoder.encode(&mut context, decision);
        }
        let mut coded = encoder.finish();
        let middle = coded.len() / 2;
        coded[middle] ^= 0x55;

        let mut decoder = MqDecoder::new(&coded);
        let mut context = Context::default();
        let got: Vec<u8> = (0..wanted.len())
            .map(|_| decoder.decode(&mut context))
            .collect();
        assert_ne!(got, wanted);
    }

    /// Several contexts at once, because every real use of this coder switches
    /// context between decisions and a single-context test would not notice a
    /// decoder that kept its state in the wrong place.
    #[test]
    fn interleaved_contexts_each_keep_their_own_state() {
        let wanted = decisions(3000);
        let mut encoder = MqEncoder::new();
        let mut contexts = [Context::default(); 19];
        for (index, &decision) in wanted.iter().enumerate() {
            encoder.encode(&mut contexts[index % 19], decision);
        }
        let coded = encoder.finish();

        let mut decoder = MqDecoder::new(&coded);
        let mut contexts = [Context::default(); 19];
        let got: Vec<u8> = (0..wanted.len())
            .map(|index| decoder.decode(&mut contexts[index % 19]))
            .collect();
        assert_eq!(got, wanted);
    }

    /// A decoder handed nothing must still answer, because a codeword segment
    /// truncated by a damaged file is decoded, not refused.
    #[test]
    fn an_empty_segment_still_produces_decisions() {
        let mut decoder = MqDecoder::new(&[]);
        let mut context = Context::new(0, 0);
        for _ in 0..64 {
            let _ = decoder.decode(&mut context);
        }
    }

    /// The raw path's one rule: a byte after `0xFF` carries seven bits.
    #[test]
    fn a_stuffed_bit_follows_every_all_ones_byte() {
        let mut decoder = RawDecoder::new(&[0xFF, 0x7F]);
        let bits: Vec<u8> = (0..15).map(|_| decoder.decode()).collect();
        assert_eq!(bits, vec![1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]);
    }
}
