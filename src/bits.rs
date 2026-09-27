//! The bit reader packet headers are written in, and the tag trees built on it.
//!
//! B.10.1: a packet header is a stream of bits, most significant first, with
//! one stuffed zero after every `0xFF` byte so that the two-byte markers a
//! codestream is scanned for can never appear inside one. Everything in a
//! packet header -- inclusion, zero bit-planes, pass counts, segment lengths --
//! is read through this one object, so the stuffing rule is written once.

/// A bit reader over a packet header.
pub struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
    current: u8,
    bits: u32,
}

impl<'a> BitReader<'a> {
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            current: 0,
            bits: 0,
        }
    }

    /// How many bytes have been consumed, counting the byte in hand.
    #[must_use]
    pub const fn consumed(&self) -> usize {
        self.position
    }

    /// Whether the reader has run past the end of the header it was given.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.position > self.data.len()
    }

    /// One bit. A reader past the end answers zero rather than refusing: a
    /// truncated header is a repair the caller reports, not a panic here.
    pub fn bit(&mut self) -> u32 {
        if self.bits == 0 {
            let previous = self.current;
            self.current = self.data.get(self.position).copied().unwrap_or(0);
            self.position += 1;
            self.bits = if previous == 0xFF { 7 } else { 8 };
        }
        self.bits -= 1;
        u32::from((self.current >> self.bits) & 1)
    }

    /// `count` bits, most significant first. `count` must be at most 32.
    pub fn bits(&mut self, count: u32) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit();
        }
        value
    }

    /// Ends the header at a byte boundary, discarding the stuffed bit when the
    /// last whole byte was `0xFF`.
    pub fn align(&mut self) {
        if self.current == 0xFF && self.bits == 0 {
            // The stuffed byte belongs to the header even though none of its
            // bits were asked for.
            self.position += 1;
        }
        self.bits = 0;
        self.current = 0;
    }
}

/// A tag tree, B.10.2.
///
/// It codes a two-dimensional array of small non-negative integers by storing,
/// at each node of a quad tree, how much larger that node's minimum is than its
/// parent's. Decoding a leaf is therefore a walk from the root, and a walk that
/// stops early -- because the answer is only known to be "greater than the
/// threshold asked about" -- is a legitimate outcome that later packets resume.
pub struct TagTree {
    /// Node values, level by level from the leaves up.
    levels: Vec<TagLevel>,
}

struct TagLevel {
    width: u32,
    height: u32,
    value: Vec<u32>,
    /// How far each node has been decoded: the value is known to be at least
    /// this much.
    lower_bound: Vec<u32>,
    known: Vec<bool>,
}

impl TagTree {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let mut levels = Vec::new();
        let (mut w, mut h) = (width.max(1), height.max(1));
        loop {
            let count = (w as usize) * (h as usize);
            levels.push(TagLevel {
                width: w,
                height: h,
                value: vec![0; count],
                lower_bound: vec![0; count],
                known: vec![false; count],
            });
            if w == 1 && h == 1 {
                break;
            }
            w = w.div_ceil(2);
            h = h.div_ceil(2);
        }
        Self { levels }
    }

    /// Decodes the leaf at `(x, y)` as far as `threshold` allows.
    ///
    /// Returns the leaf's value when it is now known to be less than
    /// `threshold`, and `None` when all that has been learnt is that it is at
    /// least `threshold` -- which is the answer "not included in this layer".
    pub fn decode(
        &mut self,
        reader: &mut BitReader,
        x: u32,
        y: u32,
        threshold: u32,
    ) -> Option<u32> {
        let mut minimum = 0;
        for depth in (0..self.levels.len()).rev() {
            let level = &mut self.levels[depth];
            let shift = u32::try_from(depth).unwrap_or(u32::MAX);
            let (lx, ly) = (x >> shift, y >> shift);
            if lx >= level.width || ly >= level.height {
                return None;
            }
            let index = (ly * level.width + lx) as usize;
            if level.lower_bound[index] < minimum {
                level.lower_bound[index] = minimum;
            }
            while !level.known[index] && level.lower_bound[index] < threshold {
                if reader.bit() == 1 {
                    level.known[index] = true;
                    level.value[index] = level.lower_bound[index];
                } else {
                    level.lower_bound[index] += 1;
                }
            }
            if !level.known[index] {
                return None;
            }
            minimum = level.value[index];
        }
        Some(minimum)
    }

    /// Decodes the leaf at `(x, y)` completely, raising the threshold until the
    /// answer is known. Used where the value itself is wanted -- the number of
    /// missing bit-planes -- rather than a yes or no about one layer.
    pub fn decode_fully(&mut self, reader: &mut BitReader, x: u32, y: u32) -> u32 {
        let mut threshold = 1;
        loop {
            if let Some(value) = self.decode(reader, x, y, threshold) {
                return value;
            }
            threshold += 1;
            // A header that never terminates the walk is damaged; the reader
            // answers zero past its end, so this cannot spin forever, but the
            // bound is stated rather than assumed.
            if reader.exhausted() && threshold > 64 {
                return threshold;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BitReader, TagTree};

    /// The stuffing rule, read the way a packet header reads it.
    #[test]
    fn a_byte_following_all_ones_carries_seven_bits() {
        let mut reader = BitReader::new(&[0xFF, 0x00, 0xFF]);
        assert_eq!(reader.bits(8), 0xFF);
        // Seven bits, not eight, come from the byte after `0xFF`.
        assert_eq!(reader.bits(7), 0x00);
        assert_eq!(reader.bits(8), 0xFF);
    }

    /// Past the end a header reads as zeroes, because a truncated packet is
    /// reported by the caller rather than trapped here.
    #[test]
    fn reading_past_the_end_yields_zeroes_and_says_so() {
        let mut reader = BitReader::new(&[0x80]);
        assert_eq!(reader.bit(), 1);
        assert_eq!(reader.bits(16), 0);
        assert!(reader.exhausted());
    }

    /// The worked example of B.10.2: the 2x3 tag tree whose leaf values are
    /// 1, 3, 2, 2, 2, 1.
    #[test]
    fn the_standards_worked_example_decodes_to_its_published_values() {
        // Built rather than transcribed: the encoder below is the definition
        // in B.10.2 run forwards, so the decoder is checked against the rule
        // and not against a copy of its own output.
        let values = [1u32, 3, 2, 2, 2, 1];
        let encoded = encode(2, 3, &values);
        let mut reader = BitReader::new(&encoded);
        let mut tree = TagTree::new(2, 3);
        let mut decoded = Vec::new();
        for y in 0..3 {
            for x in 0..2 {
                decoded.push(tree.decode_fully(&mut reader, x, y));
            }
        }
        assert_eq!(decoded, values.to_vec());
    }

    /// A tag tree asked only whether a leaf is below a threshold answers that
    /// and nothing more, and a later question resumes where the first stopped.
    #[test]
    fn a_threshold_question_can_be_asked_twice_and_resumes() {
        let values = [4u32, 0, 0, 0];
        let encoded = encode(2, 2, &values);
        let mut reader = BitReader::new(&encoded);
        let mut tree = TagTree::new(2, 2);
        assert_eq!(tree.decode(&mut reader, 0, 0, 1), None);
        assert_eq!(tree.decode(&mut reader, 0, 0, 3), None);
        assert_eq!(tree.decode(&mut reader, 0, 0, 9), Some(4));
    }

    /// The tag tree encoder of B.10.2, for the tests only.
    fn encode(width: u32, height: u32, values: &[u32]) -> Vec<u8> {
        // Build the levels, each node the minimum of its four children.
        let mut levels: Vec<(u32, u32, Vec<u32>)> = vec![(width, height, values.to_vec())];
        while {
            let (w, h, _) = levels[levels.len() - 1];
            w > 1 || h > 1
        } {
            let (w, h, ref lower) = levels[levels.len() - 1];
            let (nw, nh) = (w.div_ceil(2), h.div_ceil(2));
            let mut next = vec![u32::MAX; (nw * nh) as usize];
            for y in 0..h {
                for x in 0..w {
                    let parent = ((y / 2) * nw + x / 2) as usize;
                    next[parent] = next[parent].min(lower[(y * w + x) as usize]);
                }
            }
            levels.push((nw, nh, next));
        }

        let mut out = BitWriter::default();
        let mut emitted: Vec<Vec<bool>> = levels
            .iter()
            .map(|level| vec![false; level.2.len()])
            .collect();
        for y in 0..height {
            for x in 0..width {
                let mut minimum = 0;
                for depth in (0..levels.len()).rev() {
                    let (w, _, ref value) = levels[depth];
                    let index = ((y >> depth) * w + (x >> depth)) as usize;
                    if !emitted[depth][index] {
                        for _ in minimum..value[index] {
                            out.bit(0);
                        }
                        out.bit(1);
                        emitted[depth][index] = true;
                    }
                    minimum = value[index];
                }
            }
        }
        out.finish()
    }

    #[derive(Default)]
    struct BitWriter {
        out: Vec<u8>,
        current: u8,
        bits: u32,
        previous_was_all_ones: bool,
    }

    impl BitWriter {
        fn bit(&mut self, bit: u8) {
            let capacity = if self.previous_was_all_ones { 7 } else { 8 };
            self.current = (self.current << 1) | bit;
            self.bits += 1;
            if self.bits == capacity {
                self.out.push(self.current);
                self.previous_was_all_ones = self.current == 0xFF;
                self.current = 0;
                self.bits = 0;
            }
        }

        fn finish(mut self) -> Vec<u8> {
            while self.bits != 0 {
                self.bit(0);
            }
            self.out
        }
    }
}
