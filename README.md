# jpeg2000

A JPEG 2000 decoder in safe Rust, written from ISO/IEC 15444-1, with no
dependencies at all.

```toml
[dependencies]
jpeg2000 = { git = "https://github.com/panXDgaming/jpeg2000", tag = "v0.1.0" }
```

Not yet on crates.io.

```rust
let image = jpeg2000::decode(&bytes)?;
for component in &image.components {
    // `samples` is row-major, level-shifted, and clamped to `depth` bits.
    println!("{}x{} at {} bits", component.width, component.height, component.depth);
}
```

## What it decodes

Whole Part 1 codestreams, and the JP2 wrapper a `.jp2` file puts around one:

- tiles and tile-parts in any order, interleaved across tiles;
- all five progression orders, including the three driven by position;
- precincts, and the default precinct that is not one;
- every code-block style: selective arithmetic bypass, context reset,
  termination on every pass, vertically causal contexts, segmentation symbols;
- both wavelet filters -- the 5/3 exactly, the 9/7 in floating point;
- all three quantisation styles, and regions of interest;
- subsampled components, any bit depth to 31, signed or unsigned;
- the reversible and irreversible multiple-component transforms.

It does not decode Part 2 codestream extensions. A Part 2 *file* whose
codestream is plain Part 1 -- which is most of them -- decodes normally.

Not all of that list is equally proven. Everything above is written from the
standard; what has been checked against another decoder is what an encoder
could be made to produce -- both filters, every progression, tiles, precincts,
layers, code-block sizes, the colour transforms, and the JP2 wrapper. The
selective-bypass and terminate-all code-block styles, regions of interest, and
subsampled components are implemented and untested against a second
implementation, because no encoder to hand will emit them. That is a gap in the
evidence, not a claim about the code.

## Strict and forgiving

Real encoders break real rules. `decode` refuses a codestream that breaks any of
them. `decode_recovering` decodes it anyway and returns a list of `Repair`s
saying which. Neither one guesses silently: the difference between the two is
always visible in the result.

```rust
let image = jpeg2000::decode_recovering(&bytes)?;
for repair in &image.repairs {
    eprintln!("forgave: {repair}");
}
```

## Limits

Every size in a codestream is a declaration, and four bytes of image width can
ask for more memory than the machine has before a sample is decoded.
`decode_with_limits` and `decode_recovering_with_limits` take a ceiling; the
plain entry points use a default of sixty-seven million samples.

## How it is tested

- Three pictures built in memory, encoded losslessly by OpenJPEG, and decoded
  back to those exact pictures by `cargo test` -- no tolerance, because a
  lossless codestream has one correct decoding.
- The MQ arithmetic coder against an encoder written from the standard's other
  flowcharts, with a negative control that damages one byte and requires the
  output to change.
- The 5/3 filter against its own forward transform, at every length and both
  start parities.
- The tag tree against the encoder the standard defines for it.
- Ninety-six real and synthetic codestreams against OpenJPEG: every reversible
  one bit-identical, every irreversible one within three counts of 255.
- A fuzz target over both entry points, asserting that a returned image's
  declared size and its sample count agree.

`#![forbid(unsafe_code)]`.

## Licence

MIT or Apache-2.0, at your option.
