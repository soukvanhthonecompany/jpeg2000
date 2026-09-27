//! The JP2 file format, ISO/IEC 15444-1 Annex I.
//!
//! A `.jp2` file is a list of boxes, one of which holds the codestream. The
//! rest describe what the codestream's components mean: how many bits they
//! carry, which colour space they are in, whether one of them is an alpha
//! channel, and whether their samples are indices into a palette.
//!
//! Everything here is the wrapper only. A bare codestream is a complete image
//! without any of it, which is why [`find_codestream`] answers for both.

/// What the file says its components mean, from the `colr` box.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColourSpace {
    /// sRGB, three components.
    Rgb,
    /// Greyscale, one component.
    Greyscale,
    /// sYCC, three components.
    YCbCr,
    /// An enumerated space this decoder does not name, or an embedded ICC
    /// profile. The components are whatever the codestream carries.
    Other(u32),
}

/// What a wrapper says about the codestream inside it.
#[derive(Clone, Debug, Default)]
pub struct Container {
    pub colour_space: Option<ColourSpace>,
    /// A `cdef` box's channel-to-meaning map: for each entry, the component
    /// index and what it is (0 colour, 1 opacity, 2 premultiplied opacity).
    pub channel_definitions: Vec<(u16, u16, u16)>,
    /// A `pclr` box: one row per palette entry, one column per output channel.
    pub palette: Vec<Vec<i32>>,
    /// A `cmap` box: for each output channel, which component it comes from
    /// and which palette column, if any.
    pub component_map: Vec<(u16, u8, u8)>,
}

/// The codestream inside a JP2 wrapper, or the whole input when it is already
/// a bare codestream.
///
/// Returns the codestream's byte range and whatever the wrapper said.
#[must_use]
pub fn find_codestream(data: &[u8]) -> Option<(core::ops::Range<usize>, Container)> {
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0x4F {
        return Some((0..data.len(), Container::default()));
    }
    let mut container = Container::default();
    let mut codestream = None;
    walk(data, 0..data.len(), &mut container, &mut codestream, 0);
    codestream.map(|range| (range, container))
}

fn walk(
    data: &[u8],
    range: core::ops::Range<usize>,
    container: &mut Container,
    codestream: &mut Option<core::ops::Range<usize>>,
    depth: u32,
) {
    if depth > 8 {
        return;
    }
    let mut offset = range.start;
    while offset + 8 <= range.end {
        let length = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let length = u64::from(length);
        let kind = [
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ];
        let mut payload = offset + 8;
        let size = match length {
            // A zero length runs to the end of the file; a one means the real
            // length is the eight bytes that follow.
            0 => (range.end - offset) as u64,
            1 => {
                if offset + 16 > range.end {
                    return;
                }
                let mut wide = [0u8; 8];
                wide.copy_from_slice(&data[offset + 8..offset + 16]);
                payload = offset + 16;
                u64::from_be_bytes(wide)
            }
            other => other,
        };
        let Ok(size) = usize::try_from(size) else {
            return;
        };
        let end = offset.saturating_add(size).min(range.end);
        if payload > end {
            return;
        }
        match &kind {
            b"jp2c" => {
                if codestream.is_none() {
                    *codestream = Some(payload..end);
                }
            }
            // The header box is a container; everything below is inside it.
            b"jp2h" => walk(data, payload..end, container, codestream, depth + 1),
            b"colr" => read_colour(&data[payload..end], container),
            b"cdef" => read_channel_definitions(&data[payload..end], container),
            b"pclr" => read_palette(&data[payload..end], container),
            b"cmap" => read_component_map(&data[payload..end], container),
            _ => {}
        }
        if end <= offset {
            return;
        }
        offset = end;
    }
}

fn read_colour(segment: &[u8], container: &mut Container) {
    // I.5.3.3: method 1 names an enumerated space; any other method carries a
    // profile this decoder does not interpret.
    if segment.len() >= 7 && segment[0] == 1 {
        let value = u32::from_be_bytes([segment[3], segment[4], segment[5], segment[6]]);
        container.colour_space = Some(match value {
            16 => ColourSpace::Rgb,
            17 => ColourSpace::Greyscale,
            18 => ColourSpace::YCbCr,
            other => ColourSpace::Other(other),
        });
    }
}

fn read_channel_definitions(segment: &[u8], container: &mut Container) {
    if segment.len() < 2 {
        return;
    }
    let count = usize::from(u16::from_be_bytes([segment[0], segment[1]]));
    for index in 0..count {
        let base = 2 + index * 6;
        let Some(entry) = segment.get(base..base + 6) else {
            return;
        };
        container.channel_definitions.push((
            u16::from_be_bytes([entry[0], entry[1]]),
            u16::from_be_bytes([entry[2], entry[3]]),
            u16::from_be_bytes([entry[4], entry[5]]),
        ));
    }
}

fn read_palette(segment: &[u8], container: &mut Container) {
    if segment.len() < 3 {
        return;
    }
    let entries = usize::from(u16::from_be_bytes([segment[0], segment[1]]));
    let channels = usize::from(segment[2]);
    let Some(depths) = segment.get(3..3 + channels) else {
        return;
    };
    let widths: Vec<usize> = depths
        .iter()
        .map(|byte| (usize::from(byte & 0x7F) + 1).div_ceil(8))
        .collect();
    let signed: Vec<bool> = depths.iter().map(|byte| byte & 0x80 != 0).collect();
    let mut offset = 3 + channels;
    for _ in 0..entries {
        let mut row = Vec::with_capacity(channels);
        for (channel, width) in widths.iter().enumerate() {
            let Some(bytes) = segment.get(offset..offset + width) else {
                return;
            };
            let mut value: i64 = 0;
            for byte in bytes {
                value = (value << 8) | i64::from(*byte);
            }
            if signed[channel] {
                let bits = width * 8;
                let sign = 1i64 << (bits - 1);
                if value & sign != 0 {
                    value -= 1i64 << bits;
                }
            }
            row.push(i32::try_from(value).unwrap_or(0));
            offset += width;
        }
        container.palette.push(row);
    }
}

fn read_component_map(segment: &[u8], container: &mut Container) {
    for entry in segment.chunks_exact(4) {
        container.component_map.push((
            u16::from_be_bytes([entry[0], entry[1]]),
            entry[2],
            entry[3],
        ));
    }
}
