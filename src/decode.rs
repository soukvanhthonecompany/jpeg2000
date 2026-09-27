//! Putting the annexes together: headers, packets, blocks, wavelet, colour.

use crate::codestream::{self, Codestream, Quantisation, TileStyle, Transform};
use crate::dwt;
use crate::error::{Error, ErrorKind, Limits, Repair};
use crate::tile::{self, TileComponent};

/// One decoded component, at its own sampling.
#[derive(Clone, Debug)]
pub struct Component {
    pub width: u32,
    pub height: u32,
    /// The component's sampling period on the reference grid, `XRsiz` and
    /// `YRsiz`. A component with a period of two covers twice the area per
    /// sample, which is what chroma subsampling is.
    pub horizontal_sampling: u32,
    pub vertical_sampling: u32,
    pub depth: u8,
    pub signed: bool,
    /// Row-major, `width * height` samples, already level-shifted and clamped
    /// to the component's declared depth.
    pub samples: Vec<i32>,
}

/// A decoded image.
#[derive(Clone, Debug)]
pub struct Image {
    /// The image region on the reference grid, which is the size a viewer sees
    /// regardless of how any one component is sampled.
    pub width: u32,
    pub height: u32,
    pub components: Vec<Component>,
    /// Rules the codestream broke that decoding survived. Always empty from
    /// the strict entry point.
    pub repairs: Vec<Repair>,
}

/// Samples of one tile component, in whichever arithmetic its filter uses.
enum Plane {
    Integer(Vec<i32>),
    Real(Vec<f32>),
}

/// Decodes a parsed codestream.
pub fn decode(parsed: &Codestream, data: &[u8], limits: Limits) -> Result<Image, Error> {
    let image = &parsed.image;
    let mut repairs = parsed.repairs.clone();

    // What the header asks for, before a byte of it is allocated.
    let mut wanted: usize = 0;
    for spec in &image.components {
        let width = image.x1.div_ceil(spec.dx) - image.x0.div_ceil(spec.dx);
        let height = image.y1.div_ceil(spec.dy) - image.y0.div_ceil(spec.dy);
        wanted = (width as usize)
            .checked_mul(height as usize)
            .and_then(|samples| wanted.checked_add(samples))
            .ok_or_else(|| Error::at(0, ErrorKind::TooLarge))?;
    }
    if wanted > limits.max_samples {
        return Err(Error::at(0, ErrorKind::TooLarge));
    }

    let mut components: Vec<Component> = image
        .components
        .iter()
        .map(|spec| {
            let width = image.x1.div_ceil(spec.dx) - image.x0.div_ceil(spec.dx);
            let height = image.y1.div_ceil(spec.dy) - image.y0.div_ceil(spec.dy);
            Component {
                width,
                height,
                horizontal_sampling: spec.dx,
                vertical_sampling: spec.dy,
                depth: spec.depth,
                signed: spec.signed,
                samples: vec![0; (width as usize) * (height as usize)],
            }
        })
        .collect();
    let total: usize = components
        .iter()
        .map(|component| component.samples.len())
        .sum();
    if total == 0 {
        return Err(Error::at(0, ErrorKind::EmptyImage));
    }

    let tiles_wide = image.tiles_wide();
    for (index, style) in parsed.tiles.iter().enumerate() {
        decode_tile(
            parsed,
            data,
            limits,
            index,
            tiles_wide,
            style,
            &mut components,
            &mut repairs,
        )?;
    }

    Ok(Image {
        width: image.x1 - image.x0,
        height: image.y1 - image.y0,
        components,
        repairs,
    })
}

/// One tile: its bits gathered from every tile-part, its packets read, its
/// code-blocks decoded, its wavelet inverted, and its samples written into the
/// image.
#[allow(clippy::too_many_arguments)]
fn decode_tile(
    parsed: &Codestream,
    data: &[u8],
    limits: Limits,
    index: usize,
    tiles_wide: u32,
    style: &TileStyle,
    components: &mut [Component],
    repairs: &mut Vec<Repair>,
) -> Result<(), Error> {
    let image = &parsed.image;
    let component_count = image.components.len();
    let tile_index = u32::try_from(index).unwrap_or(0);
    let column = tile_index % tiles_wide;
    let row = tile_index / tiles_wide;
    let tx0 = (image.tile_x0 + column * image.tile_width).max(image.x0);
    let ty0 = (image.tile_y0 + row * image.tile_height).max(image.y0);
    let tx1 = (image.tile_x0 + (column + 1) * image.tile_width).min(image.x1);
    let ty1 = (image.tile_y0 + (row + 1) * image.tile_height).min(image.y1);
    if tx1 <= tx0 || ty1 <= ty0 {
        return Ok(());
    }
    let named = u16::try_from(index).unwrap_or(u16::MAX);

    // A tile's packets run across its tile-parts as one stream, so the parts
    // are joined before a single packet is read.
    let mut bits: Vec<u8> = Vec::new();
    for part in parsed
        .parts
        .iter()
        .filter(|part| part.tile as usize == index)
    {
        let end = part.end.min(data.len());
        let start = part.start.min(end);
        bits.extend_from_slice(&data[start..end]);
    }

    let mut tile_components = Vec::with_capacity(component_count);
    for component in 0..component_count {
        let built = tile::build_component(
            image,
            style,
            component,
            tile::TileRectangle {
                x0: tx0,
                y0: ty0,
                x1: tx1,
                y1: ty1,
            },
            limits,
        )
        .ok_or_else(|| Error::at(0, ErrorKind::TooLarge))?;
        tile_components.push(built);
    }

    if tile::read_packets(&bits, &mut tile_components, image, style) {
        repairs.push(Repair::PacketsRunPastTheirTile { tile: named });
    }

    let mut planes = Vec::with_capacity(component_count);
    for (number, component) in tile_components.iter_mut().enumerate() {
        let coding = style.coding.for_component(number);
        if tile::decode_blocks(component, coding.style, style.roi_shift[number]) {
            repairs.push(Repair::CodeBlockDamaged {
                tile: named,
                component: u16::try_from(number).unwrap_or(u16::MAX),
            });
        }
        planes.push(synthesise(component, style, number));
    }

    if style.coding.multiple_component_transform && planes.len() >= 3 {
        inverse_component_transform(&mut planes, style);
    }

    for (number, plane) in planes.into_iter().enumerate() {
        let Some(target) = components.get_mut(number) else {
            continue;
        };
        let spec = &image.components[number];
        let source = &tile_components[number];
        let offset_x = source.x0 - image.x0.div_ceil(spec.dx);
        let offset_y = source.y0 - image.y0.div_ceil(spec.dy);
        write_plane(target, &plane, source, offset_x, offset_y);
    }
    Ok(())
}

/// The quantisation step size of one subband, E.1.1.
fn step_size(step: codestream::StepSize, range: u32) -> f32 {
    let exponent = i32::from(step.exponent);
    let scale = 1.0 + f32::from(step.mantissa) / 2048.0;
    let shift = i32::try_from(range).unwrap_or(0) - exponent;
    // `powi` rather than a shift because the exponent may be either sign.
    2f32.powi(shift) * scale
}

/// The inverse wavelet transform of one tile component, F.3.
fn synthesise(component: &mut TileComponent, style: &TileStyle, number: usize) -> Plane {
    let coding = style.coding.for_component(number);
    let quant = &style.quant[number];
    match coding.transform {
        Transform::Reversible => Plane::Integer(synthesise_reversible(component)),
        Transform::Irreversible => {
            let reversible_quantisation = quant.style == Quantisation::None;
            Plane::Real(synthesise_irreversible(component, reversible_quantisation))
        }
    }
}

fn synthesise_reversible(component: &TileComponent) -> Vec<i32> {
    let Some(first) = component.resolutions.first() else {
        return Vec::new();
    };
    let mut current = first.bands[0].coefficients.clone();
    let mut origin = (first.x0, first.y0);
    let mut size = (first.x1 - first.x0, first.y1 - first.y0);

    for resolution in component.resolutions.iter().skip(1) {
        let width = (resolution.x1 - resolution.x0) as usize;
        let height = (resolution.y1 - resolution.y0) as usize;
        if width == 0 || height == 0 {
            // A tile component can be empty in one direction -- a tile at the
            // edge of a grid the image does not fill -- and a resolution of it
            // is then empty too. There is nothing to filter and nothing to
            // carry up to the next level.
            current = Vec::new();
            origin = (resolution.x0, resolution.y0);
            size = (0, 0);
            continue;
        }
        let mut buffer = vec![0i32; width * height];
        for v in resolution.y0..resolution.y1 {
            for u in resolution.x0..resolution.x1 {
                let selector = (u & 1) + 2 * (v & 1);
                let (bu, bv) = (u >> 1, v >> 1);
                let value = if selector == 0 {
                    sample(&current, origin, size, bu, bv)
                } else {
                    let band = &resolution.bands[(selector - 1) as usize];
                    sample(
                        &band.coefficients,
                        (band.x0, band.y0),
                        (band.width(), band.height()),
                        bu,
                        bv,
                    )
                };
                buffer[(v - resolution.y0) as usize * width + (u - resolution.x0) as usize] = value;
            }
        }
        for row in buffer.chunks_mut(width) {
            dwt::synthesise_reversible(row, resolution.x0);
        }
        let mut column = vec![0i32; height];
        for x in 0..width {
            for (y, slot) in column.iter_mut().enumerate() {
                *slot = buffer[y * width + x];
            }
            dwt::synthesise_reversible(&mut column, resolution.y0);
            for (y, value) in column.iter().enumerate() {
                buffer[y * width + x] = *value;
            }
        }
        current = buffer;
        origin = (resolution.x0, resolution.y0);
        size = (resolution.x1 - resolution.x0, resolution.y1 - resolution.y0);
    }
    current
}

fn synthesise_irreversible(component: &TileComponent, no_quantisation: bool) -> Vec<f32> {
    let Some(first) = component.resolutions.first() else {
        return Vec::new();
    };
    let dequantise = |band: &tile::Band, value: i32| -> f32 {
        if no_quantisation {
            value as f32
        } else {
            value as f32 * step_size(band.step, band.range)
        }
    };
    let low = &first.bands[0];
    let mut current: Vec<f32> = low
        .coefficients
        .iter()
        .map(|value| dequantise(low, *value))
        .collect();
    let mut origin = (first.x0, first.y0);
    let mut size = (first.x1 - first.x0, first.y1 - first.y0);

    for resolution in component.resolutions.iter().skip(1) {
        let width = (resolution.x1 - resolution.x0) as usize;
        let height = (resolution.y1 - resolution.y0) as usize;
        if width == 0 || height == 0 {
            // A tile component can be empty in one direction -- a tile at the
            // edge of a grid the image does not fill -- and a resolution of it
            // is then empty too. There is nothing to filter and nothing to
            // carry up to the next level.
            current = Vec::new();
            origin = (resolution.x0, resolution.y0);
            size = (0, 0);
            continue;
        }
        let mut buffer = vec![0f32; width * height];
        for v in resolution.y0..resolution.y1 {
            for u in resolution.x0..resolution.x1 {
                let selector = (u & 1) + 2 * (v & 1);
                let (bu, bv) = (u >> 1, v >> 1);
                let value = if selector == 0 {
                    sample(&current, origin, size, bu, bv)
                } else {
                    let band = &resolution.bands[(selector - 1) as usize];
                    dequantise(
                        band,
                        sample(
                            &band.coefficients,
                            (band.x0, band.y0),
                            (band.width(), band.height()),
                            bu,
                            bv,
                        ),
                    )
                };
                buffer[(v - resolution.y0) as usize * width + (u - resolution.x0) as usize] = value;
            }
        }
        for row in buffer.chunks_mut(width) {
            dwt::synthesise_irreversible(row, resolution.x0);
        }
        let mut column = vec![0f32; height];
        for x in 0..width {
            for (y, slot) in column.iter_mut().enumerate() {
                *slot = buffer[y * width + x];
            }
            dwt::synthesise_irreversible(&mut column, resolution.y0);
            for (y, value) in column.iter().enumerate() {
                buffer[y * width + x] = *value;
            }
        }
        current = buffer;
        origin = (resolution.x0, resolution.y0);
        size = (resolution.x1 - resolution.x0, resolution.y1 - resolution.y0);
    }
    current
}

/// Reads one sample of a rectangle that starts at `origin`, answering zero for
/// a coordinate outside it. Outside cannot happen for a well-formed
/// codestream; answering rather than panicking is what keeps a damaged one
/// from taking the process with it.
fn sample<T: Copy + Default>(
    values: &[T],
    origin: (u32, u32),
    size: (u32, u32),
    x: u32,
    y: u32,
) -> T {
    if x < origin.0 || y < origin.1 || x >= origin.0 + size.0 || y >= origin.1 + size.1 {
        return T::default();
    }
    let index = (y - origin.1) as usize * size.0 as usize + (x - origin.0) as usize;
    values.get(index).copied().unwrap_or_default()
}

/// The inverse multiple-component transform, G.2 and G.3.
///
/// Which of the two it is follows from the wavelet filter: the reversible
/// colour transform pairs with the 5/3 and the irreversible one with the 9/7,
/// because a lossless path cannot end in a rounding and a lossy one need not
/// avoid it.
fn inverse_component_transform(planes: &mut [Plane], style: &TileStyle) {
    match style.coding.default.transform {
        Transform::Reversible => {
            let (Plane::Integer(y), Plane::Integer(u), Plane::Integer(v)) =
                (&planes[0], &planes[1], &planes[2])
            else {
                return;
            };
            let count = y.len().min(u.len()).min(v.len());
            let mut red = vec![0i32; count];
            let mut green = vec![0i32; count];
            let mut blue = vec![0i32; count];
            for index in 0..count {
                let g = y[index] - (u[index] + v[index]).div_euclid(4);
                green[index] = g;
                red[index] = v[index] + g;
                blue[index] = u[index] + g;
            }
            planes[0] = Plane::Integer(red);
            planes[1] = Plane::Integer(green);
            planes[2] = Plane::Integer(blue);
        }
        Transform::Irreversible => {
            let (Plane::Real(y), Plane::Real(u), Plane::Real(v)) =
                (&planes[0], &planes[1], &planes[2])
            else {
                return;
            };
            let count = y.len().min(u.len()).min(v.len());
            let mut red = vec![0f32; count];
            let mut green = vec![0f32; count];
            let mut blue = vec![0f32; count];
            for index in 0..count {
                red[index] = y[index] + 1.402 * v[index];
                green[index] = y[index] - 0.344_136 * u[index] - 0.714_136 * v[index];
                blue[index] = y[index] + 1.772 * u[index];
            }
            planes[0] = Plane::Real(red);
            planes[1] = Plane::Real(green);
            planes[2] = Plane::Real(blue);
        }
    }
}

/// Writes one tile component into the whole component, level-shifting it.
///
/// G.1: a component that is not signed was shifted down by half its range
/// before it was transformed, and this is that shift undone. The clamp that
/// follows is not rounding error: a lossy codestream can reconstruct a sample
/// outside the range its own depth allows, and the standard says to clip it.
fn write_plane(
    target: &mut Component,
    plane: &Plane,
    source: &TileComponent,
    offset_x: u32,
    offset_y: u32,
) {
    let width = source.width() as usize;
    let height = source.height() as usize;
    let half = 1i64 << (u32::from(target.depth) - 1);
    let shift = if target.signed {
        0
    } else {
        i32::try_from(half).unwrap_or(i32::MAX)
    };
    let (low, high) = if target.signed {
        (
            i32::try_from(-half).unwrap_or(i32::MIN),
            i32::try_from(half - 1).unwrap_or(i32::MAX),
        )
    } else {
        (0, i32::try_from(half * 2 - 1).unwrap_or(i32::MAX))
    };
    for y in 0..height {
        let row = y + offset_y as usize;
        if row >= target.height as usize {
            break;
        }
        for x in 0..width {
            let column = x + offset_x as usize;
            if column >= target.width as usize {
                break;
            }
            let value = match plane {
                Plane::Integer(values) => values.get(y * width + x).copied().unwrap_or(0),
                Plane::Real(values) => {
                    let value = values.get(y * width + x).copied().unwrap_or(0.0);
                    // Round half away from zero, which is what `round` does.
                    value.round() as i32
                }
            };
            target.samples[row * target.width as usize + column] = (value + shift).clamp(low, high);
        }
    }
}
