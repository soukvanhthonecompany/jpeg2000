//! End-to-end decoding, against answers this crate did not produce.
//!
//! Each fixture was encoded losslessly by `OpenJPEG` from a picture built in
//! memory, and `fixtures/expected.rs` holds that picture. A lossless
//! codestream has one correct decoding, so these assertions are exact and
//! carry no tolerance.

mod fixtures {
    include!("fixtures/expected.rs");
}

/// Every component's samples, interleaved the way the source picture was.
fn interleaved(image: &jpeg2000::Image) -> Vec<u8> {
    let width = image.components[0].width as usize;
    let height = image.components[0].height as usize;
    let mut out = Vec::with_capacity(width * height * image.components.len());
    for y in 0..height {
        for x in 0..width {
            for component in &image.components {
                let value = component.samples[y * component.width as usize + x];
                out.push(u8::try_from(value.clamp(0, 255)).unwrap_or(0));
            }
        }
    }
    out
}

#[test]
fn a_lossless_colour_image_decodes_to_the_picture_that_was_encoded() {
    let bytes = include_bytes!("fixtures/rgb-lossless.j2k");
    let image = jpeg2000::decode(bytes).expect("a well-formed codestream is not refused");
    assert_eq!(image.width, 20);
    assert_eq!(image.height, 14);
    assert_eq!(image.components.len(), 3);
    assert!(image.repairs.is_empty());
    assert_eq!(interleaved(&image), fixtures::RGB_LOSSLESS);
}

#[test]
fn a_tiled_greyscale_image_decodes_to_the_picture_that_was_encoded() {
    let bytes = include_bytes!("fixtures/grey-tiled.j2k");
    let image = jpeg2000::decode(bytes).expect("a well-formed codestream is not refused");
    assert_eq!((image.width, image.height), (20, 14));
    assert_eq!(image.components.len(), 1);
    assert_eq!(interleaved(&image), fixtures::GREY_TILED);
}

/// Precincts and a position-driven progression at once, which is where a
/// decoder that guesses the packet order stops agreeing with everyone else.
#[test]
fn precincts_in_a_position_driven_progression_decode_in_the_right_order() {
    let bytes = include_bytes!("fixtures/rgb-precincts-rpcl.j2k");
    let image = jpeg2000::decode(bytes).expect("a well-formed codestream is not refused");
    assert_eq!(interleaved(&image), fixtures::RGB_PRECINCTS_RPCL);
}

/// The strict door refuses what the forgiving one repairs, and says which.
///
/// The rule broken here is the one every file in this project's corpus that
/// uses this codec breaks: a tile-part header declaring a number of tile-parts
/// that is not how many the tile has. `OpenJPEG` refuses those files outright,
/// so "decode it and say so" is a deliberate difference from the reference,
/// not an accident -- and this is where that difference is written down.
#[test]
fn a_wrong_tile_part_count_is_refused_strictly_and_repaired_otherwise() {
    let original = include_bytes!("fixtures/grey-tiled.j2k");
    let mut damaged = original.to_vec();
    // Find every start-of-tile-part marker segment and overstate its count.
    let mut patched = 0;
    let mut offset = 0;
    while offset + 12 < damaged.len() {
        if damaged[offset] == 0xFF && damaged[offset + 1] == 0x90 {
            damaged[offset + 11] = 9;
            patched += 1;
        }
        offset += 1;
    }
    assert!(patched > 0, "the fixture has tile-parts to damage");

    let error = jpeg2000::decode(&damaged).expect_err("strict decoding refuses the count");
    assert_eq!(error.kind, jpeg2000::ErrorKind::MalformedMarkerSegment);

    let image = jpeg2000::decode_recovering(&damaged).expect("the samples are still there");
    assert_eq!(interleaved(&image), fixtures::GREY_TILED);
    assert!(
        image.repairs.iter().any(|repair| matches!(
            repair,
            jpeg2000::Repair::TilePartCountDisagrees { declared: 9, .. }
        )),
        "the repair is reported, not silent: {:?}",
        image.repairs
    );
}

#[test]
fn something_that_is_not_a_codestream_is_refused_by_both_doors() {
    let rubbish = b"%PDF-1.7\n1 0 obj\n";
    assert_eq!(
        jpeg2000::decode(rubbish)
            .expect_err("not a codestream")
            .kind,
        jpeg2000::ErrorKind::NotACodestream
    );
    assert_eq!(
        jpeg2000::decode_recovering(rubbish)
            .expect_err("not a codestream")
            .kind,
        jpeg2000::ErrorKind::NotACodestream
    );
}

/// A codestream cut in half must not take the process with it.
#[test]
fn a_truncated_codestream_is_refused_or_decoded_but_never_panics() {
    let bytes = include_bytes!("fixtures/rgb-lossless.j2k");
    for cut in [1usize, 2, 8, 40, 60, 100, 200, 400, 800, bytes.len() - 1] {
        let _ = jpeg2000::decode(&bytes[..cut]);
        let _ = jpeg2000::decode_recovering(&bytes[..cut]);
    }
}

/// Every single-byte corruption, at a sample of positions: none may panic.
#[test]
fn a_damaged_codestream_never_panics() {
    let bytes = include_bytes!("fixtures/grey-tiled.j2k");
    for position in (0..bytes.len()).step_by(7) {
        for mask in [0xFF_u8, 0x01, 0x80] {
            let mut damaged = bytes.to_vec();
            damaged[position] ^= mask;
            let _ = jpeg2000::decode_recovering(&damaged);
        }
    }
}
