//! LuaJIT-compatible `f64` formatting.
//!
//! Rust's default `{}` formatting for `f64` diverges from LuaJIT's
//! `%.14g`-based number-to-string conversion in several ways that
//! matter for bytecode round-tripping: Rust keeps full round-trip
//! precision (so it never uses scientific notation for in-range values
//! and shows up to 17 significant digits), capitalizes `NaN`, and
//! prints exponents without the sign / zero-padding LuaJIT uses.
//!
//! [`format_lua_number`] reproduces LuaJIT 2.1's `tostring(num)`
//! output by formatting in 14-significant-digit scientific form via
//! Rust's `{:.13e}` and then reformatting as plain decimal when the
//! exponent falls in the `%g` "fixed" range `[-4, 14)`. See
//! `docs/research/luajit-number-formatting.md` for the full rules.

/// Format an `f64` the way LuaJIT's `tostring()` / `lua_tostring()`
/// would, so a parsed number constant round-trips back to the exact
/// source the compiler would emit.
///
/// Rules (matching C `printf("%.14g", v)` with LuaJIT's special-value
/// handling):
///
/// * `inf` / `-inf` / `nan` print in lowercase, no sign for NaN
///   (LuaJIT prints `nan` regardless of the NaN sign bit).
/// * `-0.0` prints as `-0` (preserved as distinct from `0`).
/// * Up to 14 significant digits; trailing zeros are stripped.
/// * Integer-valued floats print without `.0` (`3.0` → `3`).
/// * Scientific notation when the decimal exponent is `< -4` or
///   `>= 14`, with exponent formatted as `e+14`, `e-05`, `e+100`
///   (lowercase `e`, explicit sign, zero-padded to two digits).
pub fn format_lua_number(val: f64) -> String {
    // Special values first — `%e` formatting in Rust prints these as
    // "inf"/"-inf"/"NaN" but with capitalization that doesn't match
    // LuaJIT, and `-0.0` would lose its sign through the scientific
    // detour below.
    if val.is_nan() {
        // LuaJIT prints "nan" for both NaN and -NaN (the IEEE sign
        // bit is intentionally dropped).
        return "nan".to_string();
    }
    if val.is_infinite() {
        return if val.is_sign_negative() {
            "-inf".to_string()
        } else {
            "inf".to_string()
        };
    }
    if val == 0.0 {
        // Distinguishes -0.0 from +0.0 by sign bit (== treats them
        // as equal, so this branch handles both).
        return if val.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }

    // 14 significant digits in scientific form. Rust's `{:.13e}`
    // precision means 13 digits *after* the radix point, which yields
    // exactly 14 significant digits total — matching C's `%.14g`.
    let sci = format!("{:.13e}", val);

    // Split into mantissa (e.g. "3.1400000000000" or "-7.0000000000000")
    // and exponent (e.g. "0", "-5", "14"). The 'e' separator is always
    // present in `{:.N e}` output for finite values.
    let e_pos = sci
        .find('e')
        .expect("finite f64 always formats with an exponent");
    let (mantissa_raw, exp_raw) = sci.split_at(e_pos);
    let exp: i32 = exp_raw[1..]
        .parse()
        .expect("Rust scientific exponent is always a base-10 integer");

    // Strip trailing zeros from the fractional part, then any trailing
    // dot. The `%g` convention never keeps unnecessary trailing zeros.
    // Examples: "3.1400000000000" -> "3.14", "1.0000000000000" -> "1",
    // "0.0000000000000" -> "0". The integer part in `{:.13e}` form is
    // always a single digit (or sign + single digit), so this can't
    // accidentally strip a real zero from the integer part.
    let mantissa = mantissa_raw.trim_end_matches('0').trim_end_matches('.');

    if (-4..14).contains(&exp) {
        reformat_decimal(mantissa, exp)
    } else {
        format!("{}{}", mantissa, format_exponent(exp))
    }
}

/// Render a `{:.13e}`-derived mantissa + decimal exponent back into
/// plain decimal form. `mantissa` is the trimmed scientific mantissa
/// (sign + single integer digit + optional `.` + fractional digits),
/// e.g. `"3.14"`, `"-7"`, `"0"`, `"1"`.
fn reformat_decimal(mantissa: &str, exp: i32) -> String {
    // Pull off the sign so digit arithmetic below works on the bare
    // digit sequence. We reattach the sign at the end.
    let (sign, body) = if let Some(stripped) = mantissa.strip_prefix('-') {
        ("-", stripped)
    } else {
        ("", mantissa)
    };
    // Drop the radix point so we have a flat digit string. With the
    // scientific mantissa shape there's always exactly one digit
    // before the dot, so this never changes the leading digit.
    let digits: String = body.chars().filter(|c| *c != '.').collect();

    if exp >= 0 {
        // `exp` is the position of the radix point relative to the
        // start of `digits`: `exp + 1` digits fall to the left of it.
        let int_len = exp as usize + 1;
        if digits.len() >= int_len {
            // Split digits at the radix boundary. Fractional part may
            // be empty (integer-valued result — no trailing dot).
            let mut result = String::with_capacity(digits.len() + 1);
            result.push_str(&digits[..int_len]);
            if digits.len() > int_len {
                result.push('.');
                result.push_str(&digits[int_len..]);
            }
            format!("{}{}", sign, result)
        } else {
            // Value is an integer that needs trailing zeros to reach
            // the right magnitude (e.g. "1" with exp=8 -> "100000000").
            let pad = int_len - digits.len();
            format!("{}{}{}", sign, digits, "0".repeat(pad))
        }
    } else {
        // exp < 0: value is fractional, leading with
        // "0." + (-exp - 1) zeros + the digits. e.g. "1" with exp=-4
        // -> "0.0001".
        let leading_zeros = (-exp - 1) as usize;
        format!("{}0.{}{}", sign, "0".repeat(leading_zeros), digits)
    }
}

/// Format the scientific exponent per LuaJIT: lowercase `e`, explicit
/// sign (`+` or `-`), zero-padded to a minimum of two digits. Examples:
/// `14` -> `e+14`, `-5` -> `e-05`, `100` -> `e+100`.
fn format_exponent(exp: i32) -> String {
    if exp >= 0 {
        format!("e+{:02}", exp)
    } else {
        format!("e-{:02}", -exp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: assert and report both sides on failure so a
    /// formatter regression points straight at the offending input.
    fn assert_fmt(val: f64, expected: &str) {
        let actual = format_lua_number(val);
        assert_eq!(actual, expected, "format_lua_number({})", val);
    }

    // ---- Special values ------------------------------------------------

    #[test]
    fn formats_zero() {
        assert_fmt(0.0, "0");
    }

    #[test]
    fn formats_negative_zero() {
        assert_fmt(-0.0, "-0");
    }

    #[test]
    fn formats_infinity() {
        assert_fmt(f64::INFINITY, "inf");
    }

    #[test]
    fn formats_negative_infinity() {
        assert_fmt(f64::NEG_INFINITY, "-inf");
    }

    #[test]
    fn formats_nan_lowercase() {
        assert_fmt(f64::NAN, "nan");
    }

    #[test]
    fn formats_negative_nan_as_nan() {
        // IEEE NaN carries a sign bit; LuaJIT ignores it and prints
        // "nan". Construct a NaN with the sign bit set explicitly
        // (multiplying by -1 is not guaranteed to propagate the sign
        // across platforms).
        let neg_nan = f64::from_bits(f64::NAN.to_bits() | (1u64 << 63));
        assert!(neg_nan.is_sign_negative());
        assert_fmt(neg_nan, "nan");
    }

    // ---- Integer-valued floats (no trailing .0) -----------------------

    #[test]
    fn formats_integer_valued_float_without_dot() {
        assert_fmt(3.0, "3");
        assert_fmt(100.0, "100");
        assert_fmt(100_000_000.0, "100000000");
    }

    #[test]
    fn formats_negative_integer_valued_float() {
        assert_fmt(-7.0, "-7");
    }

    // ---- Plain decimal fractions --------------------------------------

    #[test]
    fn formats_pi_truncated() {
        #[allow(clippy::approx_constant)]
        let v = 3.14;
        assert_fmt(v, "3.14");
    }

    #[test]
    fn formats_repeating_decimal_at_14_sig_figs() {
        // 10.0/3.0 is the canonical case Rust formats differently
        // (it returns "3.3333333333333335" with full round-trip
        // precision). LuaJIT's %.14g rounds to 14 sig figs.
        assert_fmt(10.0 / 3.0, "3.3333333333333");
    }

    #[test]
    fn formats_small_decimal() {
        assert_fmt(0.0001, "0.0001");
        assert_fmt(0.001, "0.001");
    }

    #[test]
    fn strips_trailing_zeros_in_decimal_form() {
        // 1.5e-1 in source rounds through the formatter to 0.15.
        assert_fmt(0.15, "0.15");
    }

    #[test]
    fn formats_long_decimal_rounded_to_14_sig_figs() {
        // Source literal has more than 14 sig figs; LuaJIT rounds.
        #[allow(clippy::excessive_precision)]
        let v = 1.234_567_890_123_456_789;
        assert_fmt(v, "1.2345678901235");
    }

    // ---- Scientific notation boundaries --------------------------------

    #[test]
    fn formats_one_e14_as_scientific() {
        // Boundary: exponent 14 is OUT of the fixed range [-4, 14),
        // so 1e14 prints in scientific form.
        assert_fmt(1e14, "1e+14");
    }

    #[test]
    fn formats_one_e_minus_5_as_scientific() {
        // Boundary: exponent -5 is OUT of the fixed range, so 1e-5
        // prints in scientific form with zero-padded exponent.
        assert_fmt(1e-5, "1e-05");
    }

    #[test]
    fn formats_just_below_upper_scientific_boundary_as_decimal() {
        // 9.5e13 has decimal exponent 13 (still in [-4, 14)), so it
        // prints as a plain decimal.
        assert_fmt(9.5e13, "95000000000000");
    }

    #[test]
    fn formats_two_pow_fiftythree_at_14_sig_figs() {
        // 2^53 = 9007199254740992, which is too large for the fixed
        // range (exponent 15). LuaJIT prints it in scientific form,
        // rounded to 14 sig figs.
        assert_fmt(2.0_f64.powi(53), "9.007199254741e+15");
    }

    #[test]
    fn formats_round_up_across_scientific_boundary() {
        // 9.99999999999999 rounds to 10.0 under %.14g, so the
        // emitted value is the plain decimal "10" rather than
        // "9.9999999999999".
        assert_fmt(9.999_999_999_999_99, "10");
    }
}
