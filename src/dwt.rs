//! The inverse discrete wavelet transform, Annex F.
//!
//! Two filters, written out separately because the standard defines them
//! separately and because they are not the same kind of thing: the 5/3 is
//! integer arithmetic that inverts its forward transform exactly, and the 9/7
//! is a floating-point approximation that does not. Sharing one implementation
//! between them would mean rounding the reversible one through a float, which
//! is the one thing it exists not to do.
//!
//! Both are lifting schemes over an interleaved signal, and both read past the
//! ends of that signal, so both start by extending it symmetrically: the
//! sample one before the start is the sample one after it, mirrored.

/// The 9/7 lifting parameters, F.4.8.1. Two of them are negative, and the
/// inverse steps below subtract them as written rather than flipping signs by
/// hand, because the inverse is the forward run backwards.
const ALPHA: f32 = -1.586_134_3;
const BETA: f32 = -0.052_980_118;
const GAMMA: f32 = 0.882_911_1;
const DELTA: f32 = 0.443_506_85;
const K: f32 = 1.230_174_1;

/// How far past each end the lifting steps reach.
const EXTENSION: usize = 4;

/// The periodic symmetric extension of F.3.4: index `i` folded back into
/// `[i0, i1)` by reflection about both ends.
fn mirror(index: i64, i0: i64, i1: i64) -> usize {
    if i1 - i0 <= 1 {
        return 0;
    }
    let period = 2 * (i1 - i0 - 1);
    let mut offset = (index - i0) % period;
    if offset < 0 {
        offset += period;
    }
    if offset >= i1 - i0 {
        offset = period - offset;
    }
    usize::try_from(offset).unwrap_or(0)
}

/// One row or column of the reversible 5/3 synthesis, F.3.8.1.
///
/// `line` holds the interleaved coefficients for absolute positions
/// `i0..i0 + line.len()`, and the parity of `i0` is what decides which of them
/// are lowpass.
pub fn synthesise_reversible(line: &mut [i32], i0: u32) {
    let count = line.len();
    if count == 0 {
        return;
    }
    let start = i64::from(i0);
    let end = start + count as i64;
    if count == 1 {
        // A single sample is a lowpass sample if it sits on an even position
        // and a highpass one if it does not; the latter carries twice the gain.
        if start % 2 != 0 {
            line[0] /= 2;
        }
        return;
    }

    let mut extended = vec![0i32; count + 2 * EXTENSION];
    for (offset, slot) in extended.iter_mut().enumerate() {
        let index = start - EXTENSION as i64 + offset as i64;
        *slot = line[mirror(index, start, end)];
    }
    let base = start - EXTENSION as i64;
    let at = |index: i64| -> usize { usize::try_from(index - base).unwrap_or(0) };

    // The even (lowpass) samples first, one further out on each side than the
    // output needs, because the odd step reads them.
    let mut even = start - 2;
    if even % 2 != 0 {
        even += 1;
    }
    while even < end + 2 {
        // Saturating, not wrapping: a codestream whose coefficients do not fit
        // in an `i32` is already producing nonsense, and nonsense that stays
        // large is easier to recognise than nonsense that changes sign. No
        // well-formed codestream reaches either.
        let neighbours = extended[at(even - 1)]
            .saturating_add(extended[at(even + 1)])
            .saturating_add(2);
        let value = extended[at(even)].saturating_sub(neighbours.div_euclid(4));
        extended[at(even)] = value;
        even += 2;
    }
    let mut odd = start - 1;
    if odd % 2 == 0 {
        odd += 1;
    }
    while odd < end + 1 {
        let neighbours = extended[at(odd - 1)].saturating_add(extended[at(odd + 1)]);
        let value = extended[at(odd)].saturating_add(neighbours.div_euclid(2));
        extended[at(odd)] = value;
        odd += 2;
    }
    for (offset, slot) in line.iter_mut().enumerate() {
        *slot = extended[EXTENSION + offset];
    }
}

/// One row or column of the irreversible 9/7 synthesis, F.3.8.2.
pub fn synthesise_irreversible(line: &mut [f32], i0: u32) {
    let count = line.len();
    if count == 0 {
        return;
    }
    let start = i64::from(i0);
    let end = start + count as i64;
    if count == 1 {
        if start % 2 != 0 {
            line[0] /= 2.0;
        }
        return;
    }

    let mut extended = vec![0f32; count + 2 * EXTENSION];
    for (offset, slot) in extended.iter_mut().enumerate() {
        let index = start - EXTENSION as i64 + offset as i64;
        *slot = line[mirror(index, start, end)];
    }
    let base = start - EXTENSION as i64;
    let at = |index: i64| -> usize { usize::try_from(index - base).unwrap_or(0) };

    let first_even = if start % 2 == 0 { start } else { start + 1 };
    let first_odd = if start % 2 == 0 { start + 1 } else { start };

    // Undo the gain, then the four lifting steps in the order the forward
    // transform applied them, backwards.
    let mut index = first_even - 4;
    while index < end + 4 {
        extended[at(index)] *= K;
        index += 2;
    }
    let mut index = first_odd - 4;
    while index < end + 4 {
        extended[at(index)] /= K;
        index += 2;
    }
    // Each step reads its neighbours, so each one can be run over a window one
    // narrower than the step before it and still be right where it matters.
    for (step, (parameter, even_pass)) in
        [(DELTA, true), (GAMMA, false), (BETA, true), (ALPHA, false)]
            .into_iter()
            .enumerate()
    {
        let margin = 3 - step as i64;
        let mut index = if even_pass { first_even } else { first_odd };
        while index - 2 >= start - margin {
            index -= 2;
        }
        while index < end + margin {
            let neighbours = extended[at(index - 1)] + extended[at(index + 1)];
            extended[at(index)] -= parameter * neighbours;
            index += 2;
        }
    }
    for (offset, slot) in line.iter_mut().enumerate() {
        *slot = extended[EXTENSION + offset];
    }
}

#[cfg(test)]
mod tests {
    use super::{synthesise_irreversible, synthesise_reversible};

    /// The forward 5/3, written for the test only, so that the inverse can be
    /// checked against the definition it inverts rather than against itself.
    fn analyse_reversible(line: &[i32], i0: u32) -> Vec<i32> {
        let count = line.len();
        if count == 1 {
            return if i0.is_multiple_of(2) {
                line.to_vec()
            } else {
                vec![line[0] * 2]
            };
        }
        let start = i64::from(i0);
        let end = start + count as i64;
        let sample =
            |values: &[i32], index: i64| -> i32 { values[super::mirror(index, start, end)] };
        let mut out = line.to_vec();
        let mut odd = if start % 2 == 0 { start + 1 } else { start };
        while odd < end {
            let value =
                sample(line, odd) - (sample(line, odd - 1) + sample(line, odd + 1)).div_euclid(2);
            out[usize::try_from(odd - start).unwrap()] = value;
            odd += 2;
        }
        let highpass = out.clone();
        let mut even = if start % 2 == 0 { start } else { start + 1 };
        while even < end {
            let left = if even - 1 < start {
                sample(&highpass, even + 1)
            } else {
                sample(&highpass, even - 1)
            };
            let right = if even + 1 >= end {
                sample(&highpass, even - 1)
            } else {
                sample(&highpass, even + 1)
            };
            let value = sample(line, even) + (left + right + 2).div_euclid(4);
            out[usize::try_from(even - start).unwrap()] = value;
            even += 2;
        }
        out
    }

    /// The reversible filter's whole reason for existing: what goes in comes
    /// back out, bit for bit, at every length and at both start parities.
    #[test]
    fn the_reversible_filter_returns_exactly_what_was_transformed() {
        for length in 1..40usize {
            for start in [0u32, 1, 6, 7] {
                let original: Vec<i32> = (0..length)
                    .map(|index| {
                        let index = i32::try_from(index).unwrap();
                        (index * 37) % 211 - 105
                    })
                    .collect();
                let mut line = analyse_reversible(&original, start);
                synthesise_reversible(&mut line, start);
                assert_eq!(line, original, "length {length}, start {start}");
            }
        }
    }

    /// A constant signal has no detail, so the synthesis of a flat lowpass band
    /// and an empty highpass band must be that same constant everywhere. This
    /// is the one property of the 9/7 that holds exactly despite the floats,
    /// and it fails loudly if either gain constant is wrong.
    #[test]
    fn the_irreversible_filter_leaves_a_flat_signal_flat() {
        for length in [2usize, 3, 8, 9, 16, 31] {
            for start in [0u32, 1] {
                let mut line = vec![0f32; length];
                for (index, slot) in line.iter_mut().enumerate() {
                    let absolute = start as usize + index;
                    // The forward transform of a constant c leaves the
                    // lowpass samples at exactly c -- the gain steps cancel --
                    // and the highpass samples at zero, because a constant has
                    // no detail. So this is that, run backwards.
                    *slot = if absolute.is_multiple_of(2) {
                        100.0
                    } else {
                        0.0
                    };
                }
                synthesise_irreversible(&mut line, start);
                for value in line {
                    assert!(
                        (value - 100.0).abs() < 0.01,
                        "length {length}, start {start}: {value}"
                    );
                }
            }
        }
    }
}
