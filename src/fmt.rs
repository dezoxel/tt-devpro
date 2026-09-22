//! Java number formatting, reproduced.
//!
//! Two of this tool's three ways of printing an hour figure are JVM library
//! behaviour rather than anything the source chose, and neither has the Rust
//! equivalent it looks like. Both were measured against JDK 21 and `rustc` 1.91.1.
//!
//! - `String.format("%.Nf", v)` rounds the **shortest round-trip decimal
//!   representation** of `v` HALF_UP. Rust's `{:.N}` rounds the **exact binary
//!   value** half-to-even. Over a 188 048-value corpus drawn from this tool's own
//!   domain they disagree on 13 134 values — 6.98 %, roughly one printed line in
//!   fourteen. See C25.
//! - Interpolating a `Double` calls `Double.toString`, which keeps a `.0` on every
//!   whole number and switches to `E` notation outside `[1e-3, 1e7)`. Rust's `{}`
//!   does neither. Three of the ten distinct values in the captured
//!   `api get-worklogs` output are `1.0`, `3.0` and `5.0`. See C32.
//!
//! The third way is `ApiCommand.kt:132,180`, which prints the raw `String` the
//! option carried — `--hours 8.00` prints `8.00` — and needs no helper. All three
//! are contracts; none of them may be unified into the others.

/// Java's `String.format("%.Nf", v)`.
///
/// Verified against JDK 21 on all 188 048 values of the differential corpus.
pub fn java_fmt(v: f64, decimals: usize) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }

    let neg = v.is_sign_negative() && v != 0.0;

    // `{:?}` is the shortest round-trip representation — the same family of
    // algorithm as `Double.toString` on JDK 19+, which is what Java rounds.
    let shortest = format!("{:?}", v.abs());
    let (mant, exp) = match shortest.split_once(['e', 'E']) {
        Some((m, e)) => (m.to_string(), e.parse::<i32>().expect("exponent")),
        None => (shortest, 0),
    };
    let (int_part, frac_part) = match mant.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (mant, String::new()),
    };

    let mut digits: Vec<u8> = int_part
        .bytes()
        .chain(frac_part.bytes())
        .map(|c| c - b'0')
        .collect();
    let mut point = int_part.len() as i32 + exp;

    let cut = point + decimals as i32;
    if cut < 0 {
        let zero = if decimals == 0 {
            "0".to_string()
        } else {
            format!("0.{}", "0".repeat(decimals))
        };
        return format!("{}{}", if neg { "-" } else { "" }, zero);
    }

    let cut = cut as usize;
    if cut < digits.len() {
        let round_up = digits[cut] >= 5;
        digits.truncate(cut);
        if round_up {
            // HALF_UP on the decimal string, carrying left.
            let mut i = cut;
            loop {
                if i == 0 {
                    digits.insert(0, 1);
                    point += 1;
                    break;
                }
                i -= 1;
                if digits[i] == 9 {
                    digits[i] = 0;
                } else {
                    digits[i] += 1;
                    break;
                }
            }
        }
    } else {
        while digits.len() < cut {
            digits.push(0);
        }
    }

    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if point <= 0 {
        s.push('0');
        if decimals > 0 {
            s.push('.');
            for _ in 0..(-point) {
                s.push('0');
            }
            for d in &digits {
                s.push((b'0' + d) as char);
            }
        }
    } else {
        let p = point as usize;
        for i in 0..p {
            s.push((b'0' + digits.get(i).copied().unwrap_or(0)) as char);
        }
        if decimals > 0 {
            s.push('.');
            for i in p..(p + decimals) {
                s.push((b'0' + digits.get(i).copied().unwrap_or(0)) as char);
            }
        }
    }
    s
}

/// Java's `String.format("%W.Nf", v)` — right-aligned, space-padded.
pub fn java_fmt_width(v: f64, decimals: usize, width: usize) -> String {
    format!("{:>width$}", java_fmt(v, decimals), width = width)
}

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
    let (mant, exp) = sci.split_once('e').expect("LowerExp always emits an exponent");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `(value, %.2f, %.1f, %5.2f)` — every expectation measured on JDK 21. These
    /// are the cases where a naive `{:.N}` prints something else.
    const JVM_FMT_CASES: &[(f64, &str, &str, &str)] = &[
        (0.015, "0.02", "0.0", " 0.02"),
        (0.125, "0.13", "0.1", " 0.13"),
        (0.175, "0.18", "0.2", " 0.18"),
        (0.25, "0.25", "0.3", " 0.25"),
        (0.615, "0.62", "0.6", " 0.62"),
        (0.955, "0.96", "1.0", " 0.96"),
        (1.005, "1.01", "1.0", " 1.01"),
        (1.785, "1.79", "1.8", " 1.79"),
        (2.125, "2.13", "2.1", " 2.13"),
        (2.675, "2.68", "2.7", " 2.68"),
        (2.695, "2.70", "2.7", " 2.70"),
        (3.625, "3.63", "3.6", " 3.63"),
        (4.545, "4.55", "4.5", " 4.55"),
        (5.435, "5.44", "5.4", " 5.44"),
        (6.345, "6.35", "6.3", " 6.35"),
        (7.255, "7.26", "7.3", " 7.26"),
        (8.225, "8.23", "8.2", " 8.23"),
        (9.135, "9.14", "9.1", " 9.14"),
        (10.045, "10.05", "10.0", "10.05"),
        (11.165, "11.17", "11.2", "11.17"),
        (13.85, "13.85", "13.9", "13.85"),
        (20.95, "20.95", "21.0", "20.95"),
        (28.125, "28.13", "28.1", "28.13"),
        (35.125, "35.13", "35.1", "35.13"),
        (42.15, "42.15", "42.2", "42.15"),
        (49.25, "49.25", "49.3", "49.25"),
        (56.55, "56.55", "56.6", "56.55"),
        (63.625, "63.63", "63.6", "63.63"),
        (70.85, "70.85", "70.9", "70.85"),
        (78.05, "78.05", "78.1", "78.05"),
        (85.125, "85.13", "85.1", "85.13"),
        (92.25, "92.25", "92.3", "92.25"),
        (99.35, "99.35", "99.4", "99.35"),
        (115.25, "115.25", "115.3", "115.25"),
        (132.125, "132.13", "132.1", "132.13"),
        (148.625, "148.63", "148.6", "148.63"),
        (165.25, "165.25", "165.3", "165.25"),
        (182.125, "182.13", "182.1", "182.13"),
        (198.625, "198.63", "198.6", "198.63"),
        (215.25, "215.25", "215.3", "215.25"),
        (232.125, "232.13", "232.1", "232.13"),
        (248.625, "248.63", "248.6", "248.63"),
    ];

    #[test]
    fn matches_the_jvm_on_every_divergent_case() {
        for (v, two, one, width) in JVM_FMT_CASES {
            assert_eq!(&java_fmt(*v, 2), two, "%.2f of {v}");
            assert_eq!(&java_fmt(*v, 1), one, "%.1f of {v}");
            assert_eq!(&java_fmt_width(*v, 2, 5), width, "%5.2f of {v}");
        }
    }

    /// The point of the table: these are cases the obvious port gets wrong, so a
    /// test that both implementations pass would be testing nothing.
    #[test]
    fn the_naive_port_really_does_diverge() {
        let divergent = JVM_FMT_CASES
            .iter()
            .filter(|(v, two, one, _)| &format!("{v:.2}") != two || &format!("{v:.1}") != one)
            .count();
        assert!(divergent > 30, "only {divergent} of the table diverge");
    }

    /// Quantized values cannot hit a half-way case, which is why the renderer's
    /// own sites are safe either way and only the portal-sourced and raw-duration
    /// ones were ever at risk.
    #[test]
    fn quarter_hours_are_unambiguous() {
        for n in 0..=32 {
            let v = f64::from(n) * 0.25;
            assert_eq!(java_fmt(v, 2), format!("{v:.2}"), "quarter {v}");
        }
    }

    #[test]
    fn handles_zero_and_carry_and_sign() {
        assert_eq!(java_fmt(0.0, 2), "0.00");
        assert_eq!(java_fmt(0.0, 0), "0");
        assert_eq!(java_fmt(9.999, 2), "10.00");
        assert_eq!(java_fmt(0.999, 2), "1.00");
        assert_eq!(java_fmt(-0.125, 2), "-0.13");
        assert_eq!(java_fmt(0.004, 2), "0.00");
        assert_eq!(java_fmt(8.0, 2), "8.00");
    }

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
