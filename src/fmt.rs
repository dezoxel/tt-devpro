//! Java library behaviour the obvious Rust spelling gets wrong, reproduced.
//!
//! - Interpolating a `Double` calls `Double.toString`, which keeps a `.0` on every whole
//!   number and switches to `E` notation outside `[1e-3, 1e7)`. Rust's `{}` does neither.
//!   Three of the ten distinct values in the captured `api get-worklogs` output are `1.0`,
//!   `3.0` and `5.0`. See C32. Measured against JDK 21 and `rustc` 1.91.1; probes, raw
//!   output and the commands: `~/.cache/tt-devpro-rewrite/measurements/fmt/README.md`.
//! - `String.compareTo` orders by UTF-16 code unit, Rust's `str` by code point.
//!
//! The `%.Nf` reproduction that used to live here went with the old settle renderer. The
//! plan is not compared byte for byte with the Kotlin output, so it prints with `{:.2}`.

/// Kotlin's `"$aDouble"`, i.e. `Double.toString`.
///
/// Differs from Rust's `{}` in two ways: a whole number keeps its `.0`, and the
/// representation switches to `E` notation outside `[1e-3, 1e7)`. Only the first
/// is reachable with hours, but the second costs three lines and removes the need
/// to reason about whether it is reachable.
pub fn java_dbl(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }

    let neg = v.is_sign_negative();
    let sign = if neg { "-" } else { "" };

    // `{:e}` normalises to one digit before the point, which is the form Java's
    // "d.dddEn" is specified in.
    let sci = format!("{:e}", v.abs());
    let (mant, exp) = sci
        .split_once('e')
        .expect("LowerExp always emits an exponent");
    let exp: i32 = exp.parse().expect("exponent");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();

    if (-3..=6).contains(&exp) {
        // Plain notation, always with at least one fractional digit.
        let point = exp + 1;
        if point <= 0 {
            return format!("{sign}0.{}{digits}", "0".repeat((-point) as usize));
        }
        let point = point as usize;
        if point >= digits.len() {
            return format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()));
        }
        return format!("{sign}{}.{}", &digits[..point], &digits[point..]);
    }

    let frac = if digits.len() > 1 { &digits[1..] } else { "0" };
    format!("{sign}{}.{frac}E{exp}", &digits[..1])
}

/// `java.lang.String.compareTo` — lexicographic over UTF-16 code units.
///
/// Rust's `str` ordering compares UTF-8 bytes, i.e. code points. Measured
/// divergence: Java puts U+1F600 before U+E000, Rust puts U+E000 first.
pub fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C32's table, verbatim.
    #[test]
    fn java_dbl_matches_double_to_string() {
        assert_eq!(java_dbl(8.0), "8.0");
        assert_eq!(java_dbl(1.0), "1.0");
        assert_eq!(java_dbl(0.0), "0.0");
        assert_eq!(java_dbl(3.0), "3.0");
        assert_eq!(java_dbl(1e7), "1.0E7");
        assert_eq!(java_dbl(1.0e-4), "1.0E-4");
        for v in [0.25_f64, 0.5, 2.75, 7.5, 0.1] {
            assert_eq!(java_dbl(v), format!("{v}"), "{v} should agree with Rust");
        }
    }

    /// The live half: three of the ten distinct values in the captured
    /// `api get-worklogs` output are whole numbers, and `{}` drops their `.0`.
    #[test]
    fn java_dbl_keeps_the_trailing_zero_rust_drops() {
        for v in [1.0_f64, 3.0, 5.0] {
            assert_ne!(java_dbl(v), format!("{v}"));
            assert_eq!(java_dbl(v), format!("{v:.1}"));
        }
    }

    #[test]
    fn java_dbl_switches_to_e_notation_at_the_jvm_thresholds() {
        assert_eq!(java_dbl(0.001), "0.001");
        assert_eq!(java_dbl(9999999.0), "9999999.0");
        assert_eq!(java_dbl(1.234e-5), "1.234E-5");
        assert_eq!(java_dbl(-2.5), "-2.5");
        assert_eq!(java_dbl(-1e8), "-1.0E8");
    }
}
