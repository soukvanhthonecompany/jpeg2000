//! Decodes a JPEG 2000 file to a portable pixmap, and says what it forgave.
//!
//! Usage: `decode [--strict] <in.jp2|in.j2k> <out.pgm|out.ppm|out.raw>`
//!
//! `.raw` writes one byte per sample, components interleaved, with no header:
//! that is the form a comparison against another decoder wants, because it has
//! no format left to disagree about.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    let strict = arguments.iter().any(|argument| argument == "--strict");
    arguments.retain(|argument| argument != "--strict");
    let [input, output] = arguments.as_slice() else {
        return Err("usage: decode [--strict] <in.jp2> <out.pgm|out.ppm|out.raw>".into());
    };

    let bytes = std::fs::read(input)?;
    let image = if strict {
        jpeg2000::decode(&bytes)?
    } else {
        jpeg2000::decode_recovering(&bytes)?
    };
    for repair in &image.repairs {
        eprintln!("forgave: {repair}");
    }
    eprintln!(
        "{}x{}, {} components",
        image.width,
        image.height,
        image.components.len()
    );

    let first = image.components.first().ok_or("no components")?;
    let width = first.width as usize;
    let height = first.height as usize;
    let channels = image.components.len();
    let shift = |component: &jpeg2000::Component, value: i32| -> u8 {
        // Scaled to eight bits so that two decoders can be compared without
        // arguing about depth.
        let offset = if component.signed {
            value + (1 << (component.depth - 1))
        } else {
            value
        };
        let maximum = (1i32 << component.depth) - 1;
        let scaled = i64::from(offset.clamp(0, maximum)) * 255 / i64::from(maximum);
        u8::try_from(scaled.clamp(0, 255)).unwrap_or(0)
    };

    let mut samples = Vec::with_capacity(width * height * channels);
    for y in 0..height {
        for x in 0..width {
            for component in &image.components {
                // A subsampled component is read at its own scale.
                let cx = x * component.width as usize / width.max(1);
                let cy = y * component.height as usize / height.max(1);
                let index = cy * component.width as usize + cx;
                let value = component.samples.get(index).copied().unwrap_or(0);
                samples.push(shift(component, value));
            }
        }
    }

    let mut file = Vec::new();
    if std::path::Path::new(output)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("raw"))
    {
        file = samples;
    } else {
        let magic = if channels >= 3 { "P6" } else { "P5" };
        file.extend_from_slice(format!("{magic}\n{width} {height}\n255\n").as_bytes());
        if channels >= 3 {
            for pixel in samples.chunks(channels) {
                file.extend_from_slice(&pixel[..3]);
            }
        } else {
            file.extend(samples.iter().step_by(channels));
        }
    }
    std::fs::write(output, file)?;
    Ok(())
}
