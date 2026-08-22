//! §8.3 plain-text scalars.
//!
//! Four traps for a naive parser live here, and each of them is reachable on a conforming
//! file:
//!
//! - **Surrounding white space must be ignored** (§8.3.4), so `geometry=" 4096 : 2160 : 1 "`
//!   is valid.
//! - **A leading `+` or `-` is admitted even where the field is conceptually unsigned**
//!   (§8.3.1), so a sign is parsed and then range-checked rather than rejected outright.
//! - **Binary, octal and hexadecimal integer forms exist** (§8.3.2).
//! - **`NaN`, `+Inf` and `-Inf` are conforming float spellings** (§8.3.3), which is how a
//!   file declaring `bounds="NaN:1"` comes to exist at all.
//!
//! "White space" here is the four characters XML itself defines — space, tab, CR and LF —
//! not Rust's Unicode-aware `char::is_whitespace`, which would also accept spaces no
//! conforming writer emits and a hostile file might.

/// The four characters XML calls white space.
pub(crate) fn is_xml_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Trim §8.3.4 white space from both ends.
pub(crate) fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_ascii() && is_xml_space(c as u8))
}

/// Parse a §8.3.1/§8.3.2 integer.
///
/// One specification defect is tolerated rather than reproduced: §8.3.1's regex is
/// `\s*[+-]?[1-9][0-9]*\s*`, which admits no decimal spelling of zero at all — yet
/// `attachment:0:…` is a real and necessary location. So `0`, `+0` and `-0` are accepted.
///
/// Leading zeros on a nonzero decimal value are *not* accepted, the regex being explicit
/// that the first digit is `[1-9]`; a value like `007` would otherwise read as octal to some
/// parsers and as decimal to others.
pub(crate) fn parse_integer(text: &str) -> Option<i128> {
    let text = trim(text);
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() {
        return None;
    }

    let magnitude: i128 = if let Some(bits) = strip_radix(digits, 'b', 'B') {
        from_radix(bits, 2, |c| matches!(c, '0'..='1'))?
    } else if let Some(oct) = strip_radix(digits, 'o', 'O') {
        from_radix(oct, 8, |c| c.is_ascii_digit() && c != '8' && c != '9')?
    } else if let Some(hex) = strip_radix(digits, 'x', 'X') {
        from_radix(hex, 16, |c| c.is_ascii_hexdigit())?
    } else {
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // §8.3.1's `[1-9][0-9]*`, plus the zero the regex forgets.
        if digits.len() > 1 && digits.starts_with('0') {
            return None;
        }
        digits.parse::<i128>().ok()?
    };

    Some(if negative { -magnitude } else { magnitude })
}

/// A §8.3.1 integer that must be non-negative and fit the caller's width.
pub(crate) fn parse_u64(text: &str) -> Option<u64> {
    u64::try_from(parse_integer(text)?).ok()
}

/// A §8.3.1 integer that must be non-negative and fit a `u32`.
pub(crate) fn parse_u32(text: &str) -> Option<u32> {
    u32::try_from(parse_integer(text)?).ok()
}

fn strip_radix(digits: &str, lower: char, upper: char) -> Option<&str> {
    let rest = digits.strip_prefix('0')?;
    rest.strip_prefix(lower)
        .or_else(|| rest.strip_prefix(upper))
}

fn from_radix(digits: &str, radix: u32, valid: impl Fn(char) -> bool) -> Option<i128> {
    if digits.is_empty() || !digits.chars().all(valid) {
        return None;
    }
    i128::from_str_radix(digits, radix).ok()
}

/// Parse a §8.3.3 floating-point scalar, including the three non-numeric spellings.
///
/// `NaN`, `+Inf` and `-Inf` are conforming, so they are parsed rather than rejected — and
/// then caught downstream by the range validity rule, which is where a `bounds="NaN:1"` file
/// is supposed to fail.
pub(crate) fn parse_float(text: &str) -> Option<f64> {
    let text = trim(text);
    match text {
        "NaN" => return Some(f64::NAN),
        "+Inf" => return Some(f64::INFINITY),
        "-Inf" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // Rust's parser is more permissive than §8.3.3: it accepts `inf`, `NAN`, `1.` and a bare
    // `.` sign-only form. Reject those spellings here so a decoder reading this crate's
    // output and one reading the specification's grammar agree on what a file said.
    if !conforms_to_float_grammar(text) {
        return None;
    }
    text.parse::<f64>().ok()
}

/// `[-+]?(([0-9]+)?\.)?[0-9]+([eE][-+]?[0-9]+)?`, walked rather than regexed.
fn conforms_to_float_grammar(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        // `123.456` and `.123`; an integral part is optional, a fractional part is not.
        Some((int, frac)) => {
            int.bytes().all(|b| b.is_ascii_digit())
                && !frac.is_empty()
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
        None => !mantissa.is_empty() && mantissa.bytes().all(|b| b.is_ascii_digit()),
    };
    let exponent_ok = match exponent {
        None => true,
        Some(e) => {
            let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        }
    };
    mantissa_ok && exponent_ok
}

/// Split a colon-separated attribute — `geometry`, `bounds`, `compression` — trimming each
/// field per §8.3.4.
pub(crate) fn split_fields(text: &str) -> Vec<&str> {
    text.split(':').map(trim).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_follow_section_8_3_1_and_8_3_2() {
        assert_eq!(parse_integer(" 987 "), Some(987));
        assert_eq!(parse_integer("-123"), Some(-123));
        assert_eq!(parse_integer("+45678"), Some(45678));
        // The specification's own examples for the three radices.
        assert_eq!(parse_integer("0b10100111100101"), Some(10725));
        assert_eq!(parse_integer("0o570261"), Some(192689));
        assert_eq!(parse_integer("0x80E950AB"), Some(2_162_774_187));
        // §8.3.1's regex admits no decimal zero; `attachment:0:…` is real, so it is tolerated.
        assert_eq!(parse_integer("0"), Some(0));
        assert_eq!(parse_integer("+0"), Some(0));
        assert_eq!(parse_integer("-0"), Some(0));
        // ...but a leading zero on a nonzero value is not, since it reads as octal elsewhere.
        assert_eq!(parse_integer("007"), None);
        assert_eq!(parse_integer(""), None);
        assert_eq!(parse_integer("12a"), None);
        assert_eq!(parse_integer("0b2"), None);
        assert_eq!(parse_integer("0x"), None);
        assert_eq!(parse_integer("1.5"), None);
    }

    #[test]
    fn unsigned_conversions_range_check_after_parsing_the_sign() {
        // A sign is parsed and *then* range-checked, per §8.3.1, rather than rejected on
        // sight: that is the difference between "-1 is out of range" and "-1 is unreadable".
        assert_eq!(parse_u32("-1"), None);
        assert_eq!(parse_u32("4294967295"), Some(u32::MAX));
        assert_eq!(parse_u32("4294967296"), None);
        assert_eq!(parse_u64("18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_u64("18446744073709551616"), None);
    }

    #[test]
    fn floats_follow_section_8_3_3_including_the_three_non_numeric_spellings() {
        assert_eq!(parse_float("123"), Some(123.0));
        assert_eq!(parse_float("-123.456"), Some(-123.456));
        assert_eq!(parse_float(".123"), Some(0.123));
        assert_eq!(parse_float("+.123"), Some(0.123));
        assert_eq!(parse_float("1e0"), Some(1.0));
        assert_eq!(parse_float(" -0.123e+02 "), Some(-12.3));
        assert!(
            parse_float("NaN")
                .expect("NaN is a conforming spelling")
                .is_nan()
        );
        assert_eq!(parse_float("+Inf"), Some(f64::INFINITY));
        assert_eq!(parse_float("-Inf"), Some(f64::NEG_INFINITY));
        // Spellings Rust accepts and §8.3.3 does not.
        assert_eq!(parse_float("inf"), None);
        assert_eq!(parse_float("NAN"), None);
        assert_eq!(parse_float("nan"), None);
        assert_eq!(parse_float("Inf"), None, "the sign is mandatory on Inf");
        assert_eq!(parse_float("1."), None, "a fractional part is not optional");
        assert_eq!(parse_float("1e"), None);
        assert_eq!(parse_float(""), None);
    }

    #[test]
    fn white_space_is_xmls_four_characters_and_not_unicodes() {
        assert_eq!(trim(" \t\r\n42 \t\r\n"), "42");
        // U+00A0 NO-BREAK SPACE is white space to `char::is_whitespace` and not to XML.
        assert_eq!(parse_integer("\u{a0}42"), None);
        assert_eq!(split_fields(" 4096 : 2160 : 1 "), vec!["4096", "2160", "1"]);
    }
}
