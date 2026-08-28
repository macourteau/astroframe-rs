//! FITS header cards: parsing, folding and value lexing (FITS 4.0 §4.1, §4.2).
//!
//! Nothing here knows where the bytes came from. A caller hands over a header region and
//! gets back the keyword list; the source layer decides how the region was found and the
//! header layer decides what the keywords mean.
//!
//! Two card conventions are folded rather than reported literally, and both are folds of
//! the *assembled keyword list*: a `CONTINUE` chain (§4.2.1.2) assembles one value from
//! several cards, and a `HIERARCH` card carries its multi-word name in the card body.
//! Leaving either literal would make the list wrong rather than merely raw — the chain's
//! value would end at a card boundary, and every `HIERARCH` card would answer to the same
//! name. §4.2.1.2's two edge cases are honoured rather than smoothed over: see
//! [`fold_cards`].

use crate::error::{Error, Result};
use crate::metadata::{Keyword, KeywordOrigin, ValueKind, collapse_whitespace};

/// Where a card-level fault was found: the card's ordinal in the header region, counted from
/// one, and the keyword name the card carries.
///
/// § Errors requires a reason to name what was expected, what was found, and **where** when
/// the parser knows it — and this parser does know. The fold counts cards in order to enforce
/// the card cap, so the ordinal is already in hand, and a consumer whose documented move is
/// skip-and-log has nothing to grep an 8 MiB header for when the sentence ends at "outside the
/// FITS header character set".
///
/// The name is carried as bytes and decoded in [`Display`](std::fmt::Display), so a card that
/// parses cleanly pays nothing for the context a card that does not will name.
#[derive(Clone, Copy)]
struct At<'a> {
    ordinal: u64,
    name: &'a [u8],
}

impl std::fmt::Display for At<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "card {} ({})",
            self.ordinal,
            String::from_utf8_lossy(self.name)
        )
    }
}

/// One card, in bytes.
const CARD: usize = 80;
/// The keyword-name field, bytes 0 through 7.
const NAME: usize = 8;

/// A FITS header unit is a whole number of these, and so is a data unit once padded.
pub(crate) const BLOCK: usize = 2880;

/// True if this 2880-byte block contains an `END` card.
pub(crate) fn block_has_end(block: &[u8]) -> bool {
    cards(block).any(|card| field(card, 0, NAME).trim_ascii() == b"END")
}

/// Parse and fold a complete FITS header region into keywords.
///
/// `region` is a whole number of 2880-byte blocks whose last block contains `END`.
/// Cards at and after `END` are not reported.
///
/// The `CONTINUE` rule is deliberately stricter than the widespread unconditional fold,
/// which strips a character from the previous value without checking that it ends in `&`.
/// On conforming input the two agree. Here, a string value ending in `&` that is *not*
/// immediately followed by a conforming `CONTINUE` record keeps the `&` as a literal final
/// character, and a `CONTINUE` record that continues nothing is reported as commentary
/// text under the name `CONTINUE` rather than corrupting whatever preceded it.
pub(crate) fn fold_cards(
    region: &[u8],
    origin: KeywordOrigin,
    max_cards: u32,
    max_value_bytes: u64,
) -> Result<Vec<Keyword>> {
    let mut out: Vec<Keyword> = Vec::new();
    let mut chain: Option<Chain> = None;
    let mut seen: u64 = 0;
    let mut ended = false;

    for card in cards(region) {
        seen += 1;
        if seen > u64::from(max_cards) {
            return Err(Error::limit(format!(
                "FITS header has more than {max_cards} cards"
            )));
        }

        let name_field = field(card, 0, NAME);
        let trimmed_name = name_field.trim_ascii();
        if trimmed_name == b"END" {
            ended = true;
            break;
        }

        // A `CONTINUE` record has no value indicator, so its value starts at byte 8.
        if trimmed_name == b"CONTINUE" {
            let body = field(card, NAME, CARD);
            let (value_part, comment_part) = split_value_comment(body);
            let continued = match (parse_quoted(value_part), chain.as_ref()) {
                (Some(text), Some(state)) if continuable(state) => Some((text, state)),
                _ => None,
            };
            match continued {
                Some((text, _)) => {
                    check_ascii(
                        value_part,
                        "value",
                        At {
                            ordinal: seen,
                            name: trimmed_name,
                        },
                    )?;
                    let comment = comment_text(comment_part);
                    let index = chain
                        .as_ref()
                        .expect("continued implies an open chain")
                        .index;
                    let seed = out[index].value().to_owned();
                    let state = chain.as_mut().expect("continued implies an open chain");
                    let value = state.value.get_or_insert(seed);
                    value.pop(); // the `&` this record continues from
                    value.push_str(&text);
                    // Checked as the value grows rather than once at the end, so the buffer
                    // stays bounded instead of being refused after it is built. FITS reaches
                    // this only in principle -- the card cap bounds a chain to about 287 KB,
                    // linear in the input -- but the rule is the same on both surfaces, which
                    // is what *A keyword reads the same from either container* asks for.
                    if value.len() as u64 > max_value_bytes {
                        return Err(Error::limit(format!(
                            "assembled keyword value: a CONTINUE chain assembles more than \
                             the {max_value_bytes} bytes the cap allows"
                        )));
                    }
                    // §4.2.1.2 leaves the comment of an assembled keyword open. This takes
                    // the last continued record's comment when it has one and the first
                    // card's otherwise, so a chain that comments only its opening card keeps
                    // that comment. `None` *is* the fallback — `close_chain` resolves it —
                    // so the opening card's comment is never copied.
                    state.comment = comment;
                    if !value.ends_with('&') {
                        // The chain is complete: pack it once, here.
                        let closed = chain.take().expect("just borrowed");
                        close_chain(&mut out, &closed);
                    }
                }
                None => {
                    // Edge case 3: an orphaned record is commentary text. It is also where
                    // a non-conforming `CONTINUE` lands, which is why the value it failed
                    // to continue keeps its `&` — nothing above touched it.
                    out.push(commentary("CONTINUE", body, origin));
                    if let Some(closed) = chain.take() {
                        close_chain(&mut out, &closed);
                    }
                }
            }
            continue;
        }

        // Any other card breaks the chain: §4.2.1.2 continues an *immediately* following
        // record, and a blank card between two halves of a string is still a card. A chain
        // broken this way is still assembled — its accumulated value is what the keyword
        // reports — so it is packed on the way out rather than discarded.
        if let Some(closed) = chain.take() {
            close_chain(&mut out, &closed);
        }

        if let Some((name, body)) = hierarch(card, seen)? {
            // The multi-word name, not the bare `HIERARCH` the name field holds: that is the
            // name this card is reported and looked up under.
            let at = At {
                ordinal: seen,
                name: name.as_bytes(),
            };
            let (value, comment, kind) = parse_value(body, at)?;
            push(&mut out, &mut chain, &name, value, comment, kind, origin);
            continue;
        }

        let at = At {
            ordinal: seen,
            name: trimmed_name,
        };
        check_ascii(name_field, "keyword name", at)?;
        // Bound, not owned. `check_ascii` above has just proved every byte is `0x20..=0x7e`,
        // so the `Cow` is `Borrowed` and `into_owned` was a guaranteed copy of a name
        // `Keyword::new` then copies again into its packed buffer.
        let name = String::from_utf8_lossy(trimmed_name);

        if field(card, NAME, NAME + 2) == b"= " {
            let (value, comment, kind) = parse_value(field(card, NAME + 2, CARD), at)?;
            push(&mut out, &mut chain, &name, value, comment, kind, origin);
        } else {
            let body = field(card, NAME, CARD);
            let text = trim_end_lossy(body);
            if name.is_empty() && text.is_empty() {
                continue; // a blank card carries nothing to report
            }
            out.push(Keyword::new(
                &name,
                "",
                Some(&text),
                origin,
                ValueKind::Commentary,
            ));
        }
    }

    // A chain still open when the cards run out — an `END` reached mid-chain, or a header
    // whose last card leaves one open — is assembled from what it accumulated.
    if let Some(closed) = chain.take() {
        close_chain(&mut out, &closed);
    }

    if !ended {
        return Err(Error::malformed("FITS header region has no END card"));
    }
    Ok(out)
}

/// Lex an integer-valued keyword. `None` if the text is not a conforming integer value.
///
/// §4.2.3, and nothing beyond it: no radix prefix, no digit separator, no surrounding
/// blanks — a valued card's text arrives already trimmed. A value too large for `i64`
/// is `None` rather than a wrapped number; [`lex_number`] is what reads it.
pub(crate) fn lex_integer(value: &str) -> Option<i64> {
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Lex a real-valued keyword, accepting FITS's `D` exponent character as well as `E`.
/// `None` if the text is not a conforming numeric value.
///
/// §4.2.4's grammar is checked here rather than delegated: Rust's float parser accepts
/// `inf`, `NaN` and `1_0`, none of which are FITS values. An integer-spelled value is a
/// valid real value, which is what reads the unsigned-64 convention's `BZERO` of 2^63 —
/// it exceeds `i64::MAX` and has no integer lexing.
pub(crate) fn lex_number(value: &str) -> Option<f64> {
    let bytes = value.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i = 1;
    }
    let mantissa_start = i;
    while matches!(bytes.get(i), Some(b) if b.is_ascii_digit()) {
        i += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while matches!(bytes.get(i), Some(b) if b.is_ascii_digit()) {
            i += 1;
        }
    }
    // At least one digit, wherever the decimal point sits.
    if !value
        .get(mantissa_start..i)?
        .bytes()
        .any(|b| b.is_ascii_digit())
    {
        return None;
    }
    if matches!(bytes.get(i), Some(b'E' | b'e' | b'D' | b'd')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exponent_start = i;
        while matches!(bytes.get(i), Some(b) if b.is_ascii_digit()) {
            i += 1;
        }
        if i == exponent_start {
            return None;
        }
    }
    if i != bytes.len() {
        return None;
    }
    // The rewrite only where there is something to rewrite: `D` is the rare spelling, and an
    // ordinary `1.234E+02` — or a plain integer — would otherwise allocate a copy of itself to
    // hand to a parser that would have taken the original.
    let parsed: f64 = match value.contains(['D', 'd']) {
        true => value.replace(['D', 'd'], "E").parse().ok()?,
        false => value.parse().ok()?,
    };
    // A grammatical value whose magnitude no `f64` holds is not a value this can report.
    parsed.is_finite().then_some(parsed)
}

/// Lex a logical-valued keyword (`T` / `F`). `None` otherwise.
pub(crate) fn lex_logical(value: &str) -> Option<bool> {
    match value.trim() {
        "T" => Some(true),
        "F" => Some(false),
        _ => None,
    }
}

/// The open end of a `CONTINUE` chain: which keyword is being assembled, and the comment
/// its first card carried.
struct Chain {
    index: usize,
    first_comment: Option<String>,
    /// The value assembled so far, and **`None` until a continuation actually arrives**.
    ///
    /// Held here rather than rebuilt in `out[index]` on every record: a `Keyword` packs its
    /// three texts into one allocation, so writing each continuation back copies the whole
    /// accumulated value again — quadratic in the chain length, and the card cap allows 4096
    /// of them, measured at 465 MB from a 161 KB header. Lazy because *opening* a chain must
    /// cost nothing either: every string card ending in `&` opens one, and most are never
    /// continued.
    value: Option<String>,
    /// The last continued card's comment, by §4.2.1.2's precedence — and **`None` until a
    /// continuation that carries one actually arrives**, which is `first_comment`.
    ///
    /// Lazy for the same reason `value` is: opening a chain must cost nothing. A FITS comment
    /// is at most 70 bytes so this is not the megabyte its XISF twin was, but the two surfaces
    /// say the same thing, and one of them saying it differently is how the shape was missed
    /// there.
    comment: Option<String>,
}

/// The cards of a region. A trailing partial card is not a card.
///
/// `as_chunks` rather than `chunks_exact`: the chunk width is a constant, so the array form
/// carries it in the type and drops the per-iteration length check. The remainder — a trailing
/// partial card — is discarded, which is the same answer `chunks_exact` gave.
fn cards(region: &[u8]) -> impl Iterator<Item = &[u8; CARD]> {
    region.as_chunks::<CARD>().0.iter()
}

/// `bytes[from..to]`, clamped — this module never indexes hostile input.
fn field(bytes: &[u8], from: usize, to: usize) -> &[u8] {
    let to = to.min(bytes.len());
    let from = from.min(to);
    &bytes[from..to]
}

/// Whether the keyword a chain points at is still open.
fn continuable(state: &Chain) -> bool {
    // An un-continued chain has no accumulated value yet; it was opened because the card's own
    // value ends in `&`, which is the condition, so it is continuable. `index` needs no check:
    // `push` sets it to `out.len()` immediately before pushing, so it always addresses a
    // keyword — an assertion the old shape carried as a live test and this one does not need.
    match &state.value {
        Some(v) => v.ends_with('&'),
        None => true,
    }
}

/// Write an assembled chain back into the keyword it started from, packing it once.
fn close_chain(out: &mut [Keyword], state: &Chain) {
    // Nothing to write when no continuation arrived: the card already holds its own value, and
    // rebuilding it would allocate a buffer identical to the one it has.
    let Some(value) = state.value.as_deref() else {
        return;
    };
    // §4.2.1.2's precedence, resolved here rather than carried: the last continued card's
    // comment when it had one, and the opening card's otherwise.
    let comment = state.comment.as_deref().or(state.first_comment.as_deref());
    if let Some(kw) = out.get_mut(state.index) {
        // A chain assembles character strings and nothing else — `push` opens one only for a
        // string value — so the assembled keyword's kind is the opening card's.
        *kw = Keyword::new(kw.name(), value, comment, kw.origin(), kw.value_kind());
    }
}

/// Record a valued card, opening a chain when its value is a string left open with `&`.
fn push(
    out: &mut Vec<Keyword>,
    chain: &mut Option<Chain>,
    name: &str,
    value: String,
    comment: Option<String>,
    kind: ValueKind,
    origin: KeywordOrigin,
) {
    // §4.2.1.2 continues character strings only, so a numeric value ending in `&` is a
    // value ending in `&`.
    let opens = kind == ValueKind::CharacterString && value.ends_with('&');
    let index = out.len();
    out.push(Keyword::new(name, &value, comment.as_deref(), origin, kind));
    if opens {
        // The comment moves into the chain rather than being cloned into it: the card's own
        // copy is already packed into the `Keyword` pushed above.
        *chain = Some(Chain {
            index,
            first_comment: comment,
            value: None,
            comment: None,
        });
    }
}

/// A commentary keyword: empty value, card body as the comment.
fn commentary(name: &str, body: &[u8], origin: KeywordOrigin) -> Keyword {
    Keyword::new(
        name,
        "",
        Some(&trim_end_lossy(body)),
        origin,
        ValueKind::Commentary,
    )
}

/// The multi-word name and value body of a `HIERARCH` card, if this is one.
///
/// `HIERARCH` appears nowhere in FITS 4.0; it is an ESO convention that carries the name
/// in the card body, up to the first `=`. That whole span is a keyword-name field for the
/// character-set rule, since it ends up as a lookup key.
fn hierarch(card: &[u8], ordinal: u64) -> Result<Option<(String, &[u8])>> {
    if field(card, 0, NAME) != b"HIERARCH" {
        return Ok(None);
    }
    let body = field(card, NAME, CARD);
    let Some(offset) = body.iter().position(|&b| b == b'=') else {
        return Ok(None);
    };
    let lossy = String::from_utf8_lossy(field(body, 0, offset));
    // The collapse is what allocates, so the two conditions that discard its result are asked
    // first: a name field holding nothing but blanks, and one holding a byte the character set
    // forbids. `HIERARCH = value` names nothing; it is an ordinary keyword named `HIERARCH`.
    if lossy.split_whitespace().next().is_none() {
        return Ok(None);
    }
    check_ascii(
        field(card, 0, NAME + offset),
        "keyword name",
        At {
            ordinal,
            name: b"HIERARCH",
        },
    )?;
    Ok(Some((
        collapse_whitespace(&lossy),
        field(body, offset + 1, body.len()),
    )))
}

/// Parse the text after a value indicator into `(value, comment, kind)`.
///
/// The kind is [`ValueKind::CharacterString`] exactly when the value field opened with a
/// quote, which is §4.2.1's own criterion, and [`ValueKind::Other`] for everything else. The
/// unquoted kinds are not told apart here: §4.2.2 through §4.2.6 are a parse of the text, and
/// the text is what is reported.
fn parse_value(body: &[u8], at: At<'_>) -> Result<(String, Option<String>, ValueKind)> {
    let (value_part, comment_part) = split_value_comment(body);
    check_ascii(value_part, "value", at)?;
    let comment = comment_text(comment_part);
    if value_part.trim_ascii_start().first() == Some(&b'\'') {
        let value = parse_quoted(value_part).ok_or_else(|| {
            Error::malformed(format!("unterminated FITS character-string value in {at}"))
        })?;
        Ok((value, comment, ValueKind::CharacterString))
    } else {
        Ok((
            String::from_utf8_lossy(value_part.trim_ascii()).into_owned(),
            comment,
            ValueKind::Other,
        ))
    }
}

/// Split a card body at the comment separator: the first `/` outside a quoted string.
fn split_value_comment(body: &[u8]) -> (&[u8], Option<&[u8]>) {
    let mut quoted = false;
    let mut i = 0;
    while let Some(&b) = body.get(i) {
        if quoted {
            if b == b'\'' {
                if body.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                quoted = false;
            }
        } else if b == b'\'' {
            quoted = true;
        } else if b == b'/' {
            return (field(body, 0, i), Some(field(body, i + 1, body.len())));
        }
        i += 1;
    }
    (body, None)
}

/// Unquote a character-string value (§4.2.1). `None` if the text does not open with a
/// quote or never closes it.
fn parse_quoted(value_part: &[u8]) -> Option<String> {
    let text = value_part.trim_ascii_start();
    if text.first() != Some(&b'\'') {
        return None;
    }
    let mut content: Vec<u8> = Vec::new();
    let mut i = 1;
    let mut closed = false;
    while let Some(&b) = text.get(i) {
        if b == b'\'' {
            if text.get(i + 1) == Some(&b'\'') {
                content.push(b'\'');
                i += 2;
                continue;
            }
            closed = true;
            break;
        }
        content.push(b);
        i += 1;
    }
    if !closed {
        return None;
    }
    // Trailing blanks are not significant; leading blanks are.
    Some(String::from_utf8_lossy(content.trim_ascii_end()).into_owned())
}

/// The comment a card carries, if it carries one at all.
fn comment_text(comment_part: Option<&[u8]>) -> Option<String> {
    comment_part.map(|text| String::from_utf8_lossy(text).trim().to_owned())
}

/// Decode text that the character-set rule tolerates: lossily, right-trimmed only.
fn trim_end_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.trim_ascii_end()).into_owned()
}

/// Reject a byte outside the header character set (§4.1) in a field where it cannot be
/// tolerated — a name that cannot be represented cannot be matched, and a value that
/// cannot be read as the standard defines it cannot be trusted downstream.
fn check_ascii(bytes: &[u8], field_name: &str, at: At<'_>) -> Result<()> {
    match bytes.iter().find(|&&b| !(0x20..=0x7e).contains(&b)) {
        Some(&b) => Err(Error::malformed(format!(
            "byte 0x{b:02x} outside the FITS header character set in a keyword {field_name} in \
             {at}"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One 80-byte card, space-padded.
    fn card(text: &str) -> Vec<u8> {
        raw(text.as_bytes())
    }

    /// One 80-byte card from bytes, so a test can place a byte the character set forbids.
    fn raw(bytes: &[u8]) -> Vec<u8> {
        assert!(bytes.len() <= CARD, "card longer than 80 bytes");
        let mut out = bytes.to_vec();
        out.resize(CARD, b' ');
        out
    }

    /// The cards, an `END`, and blank fill to the block boundary.
    fn region(cards: &[Vec<u8>]) -> Vec<u8> {
        let mut out: Vec<u8> = cards.concat();
        out.extend_from_slice(&card("END"));
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, b' ');
        out
    }

    fn fold(cards: &[Vec<u8>]) -> Vec<Keyword> {
        fold_cards(&region(cards), KeywordOrigin::Image, 1000, 1 << 20).expect("header parses")
    }

    fn fold_err(cards: &[Vec<u8>]) -> Error {
        fold_cards(&region(cards), KeywordOrigin::Image, 1000, 1 << 20)
            .expect_err("header is refused")
    }

    fn get<'a>(keywords: &'a [Keyword], name: &str) -> Option<&'a Keyword> {
        keywords.iter().find(|kw| kw.name() == name)
    }

    #[test]
    fn valued_card_splits_name_value_and_comment() {
        let kws = fold(&[card("EXPTIME =                300.0 / exposure, seconds")]);
        assert_eq!(kws.len(), 1);
        assert_eq!(kws[0].name(), "EXPTIME");
        assert_eq!(kws[0].value(), "300.0");
        assert_eq!(kws[0].comment(), Some("exposure, seconds"));
        assert_eq!(kws[0].origin(), KeywordOrigin::Image);
    }

    #[test]
    fn a_card_without_a_comment_has_none_not_empty() {
        let kws = fold(&[card("BITPIX  =                   16")]);
        assert_eq!(kws[0].value(), "16");
        assert_eq!(kws[0].comment(), None);
    }

    #[test]
    fn a_slash_inside_a_quoted_string_is_not_a_comment() {
        let kws = fold(&[card("FILENAME= '/data/raw/m31.fit' / on disk")]);
        assert_eq!(kws[0].value(), "/data/raw/m31.fit");
        assert_eq!(kws[0].comment(), Some("on disk"));

        let kws = fold(&[card("FILENAME= '/data/raw/m31.fit'")]);
        assert_eq!(kws[0].value(), "/data/raw/m31.fit");
        assert_eq!(kws[0].comment(), None);
    }

    #[test]
    fn a_doubled_quote_is_one_literal_quote() {
        let kws = fold(&[card("OBJECT  = 'Barnard''s Loop' / catalogue name")]);
        assert_eq!(kws[0].value(), "Barnard's Loop");
        assert_eq!(kws[0].comment(), Some("catalogue name"));
    }

    #[test]
    fn string_blanks_are_stripped_at_the_end_and_kept_at_the_start() {
        let kws = fold(&[card("OBJECT  = '  M31   '")]);
        assert_eq!(kws[0].value(), "  M31");
    }

    #[test]
    fn commentary_cards_keep_their_text_and_document_order() {
        let kws = fold(&[
            card("COMMENT first comment"),
            card("HISTORY first history"),
            card("COMMENT second comment"),
            card("HISTORY second history"),
        ]);
        let texts: Vec<(&str, Option<&str>, &str)> = kws
            .iter()
            .map(|kw| (kw.name(), kw.comment(), kw.value()))
            .collect();
        assert_eq!(
            texts,
            vec![
                ("COMMENT", Some("first comment"), ""),
                ("HISTORY", Some("first history"), ""),
                ("COMMENT", Some("second comment"), ""),
                ("HISTORY", Some("second history"), ""),
            ]
        );
    }

    #[test]
    fn a_blank_name_card_with_text_is_commentary_and_an_empty_one_is_dropped() {
        let kws = fold(&[
            raw(b"        loose commentary text"),
            card(""),
            card("BITPIX  =                   16"),
        ]);
        assert_eq!(kws.len(), 2);
        assert_eq!(kws[0].name(), "");
        assert_eq!(kws[0].comment(), Some("loose commentary text"));
        assert_eq!(kws[1].name(), "BITPIX");
    }

    #[test]
    fn names_are_stored_exactly_and_do_not_case_fold() {
        let kws = fold(&[card("SITELAT =                12.34")]);
        assert_eq!(get(&kws, "SITELAT").map(Keyword::value), Some("12.34"));
        assert!(get(&kws, "sitelat").is_none());
    }

    #[test]
    fn a_continue_chain_assembles_under_the_first_cards_name() {
        let kws = fold(&[
            card("LONGSTR = 'the first part &'"),
            card("CONTINUE  'the second part &'"),
            card("CONTINUE  'and the last part'"),
        ]);
        assert_eq!(kws.len(), 1);
        assert_eq!(kws[0].name(), "LONGSTR");
        assert_eq!(
            kws[0].value(),
            "the first part the second part and the last part"
        );
        assert!(!kws[0].value().ends_with('&'));
    }

    #[test]
    fn a_chain_takes_the_last_records_comment_and_falls_back_to_the_first_cards() {
        let kws = fold(&[
            card("LONGSTR = 'part one &' / opening comment"),
            card("CONTINUE  'part two' / closing comment"),
        ]);
        assert_eq!(kws[0].comment(), Some("closing comment"));

        let kws = fold(&[
            card("LONGSTR = 'part one &' / opening comment"),
            card("CONTINUE  'part two'"),
        ]);
        assert_eq!(kws[0].comment(), Some("opening comment"));
    }

    #[test]
    fn a_dangling_ampersand_stays_literal() {
        // Edge case 2: nothing conforming follows, so the `&` is part of the value.
        let kws = fold(&[
            card("LONGSTR = 'no continuation &'"),
            card("BITPIX  =                   16"),
        ]);
        assert_eq!(kws[0].value(), "no continuation &");
        assert_eq!(kws[1].name(), "BITPIX");
    }

    #[test]
    fn a_non_conforming_continue_neither_continues_nor_is_dropped() {
        // Its body is not a quoted string, so edge case 2 applies to the card above it and
        // edge case 3 to the record itself.
        let kws = fold(&[
            card("LONGSTR = 'no continuation &'"),
            card("CONTINUE  not a quoted string"),
        ]);
        assert_eq!(kws.len(), 2);
        assert_eq!(kws[0].value(), "no continuation &");
        assert_eq!(kws[1].name(), "CONTINUE");
        assert_eq!(kws[1].value(), "");
        assert_eq!(kws[1].comment(), Some("  not a quoted string"));
    }

    #[test]
    fn an_orphaned_continue_is_commentary() {
        // Edge case 3, with a numeric predecessor: §4.2.1.2 continues strings only.
        let kws = fold(&[
            card("BSCALE  =                  1.0"),
            card("CONTINUE  'orphaned text' / stray"),
        ]);
        assert_eq!(kws.len(), 2);
        assert_eq!(kws[1].name(), "CONTINUE");
        assert_eq!(kws[1].value(), "");
        assert_eq!(kws[1].comment(), Some("  'orphaned text' / stray"));
    }

    #[test]
    fn a_card_between_the_halves_breaks_the_chain() {
        let kws = fold(&[
            card("LONGSTR = 'first half &'"),
            card("COMMENT interrupting card"),
            card("CONTINUE  'second half'"),
        ]);
        assert_eq!(kws.len(), 3);
        assert_eq!(kws[0].value(), "first half &");
        assert_eq!(kws[2].name(), "CONTINUE");
    }

    #[test]
    fn hierarch_cards_carry_their_full_names_and_do_not_collide() {
        let kws = fold(&[
            card("HIERARCH ESO DET EXP = 12 / c"),
            card("HIERARCH ESO DET CHIP  ID = 'ccd65' / the detector"),
        ]);
        assert_eq!(kws.len(), 2);
        assert_eq!(kws[0].name(), "ESO DET EXP");
        assert_eq!(kws[0].value(), "12");
        assert_eq!(kws[0].comment(), Some("c"));
        assert_eq!(kws[1].name(), "ESO DET CHIP ID");
        assert_eq!(kws[1].value(), "ccd65");
        assert_ne!(kws[0].name(), kws[1].name());
    }

    #[test]
    fn a_hierarch_card_without_an_equals_sign_is_commentary() {
        let kws = fold(&[card("HIERARCH ESO DET EXP")]);
        assert_eq!(kws[0].name(), "HIERARCH");
        assert_eq!(kws[0].value(), "");
    }

    #[test]
    fn non_ascii_in_a_commentary_card_is_tolerated_lossily() {
        let mut bytes = b"COMMENT seeing was poor, caf".to_vec();
        bytes.push(0xe9); // Latin-1 'e' acute: not valid UTF-8 on its own
        let kws = fold(&[raw(&bytes), card("BITPIX  =                   16")]);
        assert_eq!(kws[0].name(), "COMMENT");
        assert!(
            kws[0].comment().is_some_and(|c| c.contains('\u{fffd}')),
            "expected a replacement character, got {:?}",
            kws[0].comment()
        );
        assert_eq!(kws[1].value(), "16");
    }

    #[test]
    fn non_ascii_in_a_cards_comment_field_is_tolerated_lossily() {
        let mut bytes = b"EXPTIME =                300.0 / caf".to_vec();
        bytes.push(0xe9);
        let kws = fold(&[raw(&bytes)]);
        assert_eq!(kws[0].value(), "300.0");
        assert_eq!(kws[0].comment(), Some("caf\u{fffd}"));
    }

    #[test]
    fn non_ascii_in_a_value_field_is_a_hard_error() {
        let mut bytes = b"OBJECT  = 'M".to_vec();
        bytes.push(0xe9);
        bytes.extend_from_slice(b"31'");
        assert!(matches!(fold_err(&[raw(&bytes)]), Error::Malformed(_)));
    }

    #[test]
    fn non_ascii_in_a_keyword_name_field_is_a_hard_error() {
        let mut bytes = b"OBJ".to_vec();
        bytes.push(0xe9);
        bytes.extend_from_slice(b"CT  =                   16");
        assert!(matches!(fold_err(&[raw(&bytes)]), Error::Malformed(_)));
    }

    #[test]
    fn non_ascii_in_a_hierarch_name_is_a_hard_error() {
        let mut bytes = b"HIERARCH ESO D".to_vec();
        bytes.push(0xe9);
        bytes.extend_from_slice(b"T EXP = 12 / c");
        assert!(matches!(fold_err(&[raw(&bytes)]), Error::Malformed(_)));
    }

    #[test]
    fn values_are_reported_as_written_and_never_reformatted() {
        let kws = fold(&[
            card("BSCALE  =              3.0E+02 / powers of ten"),
            card("BZERO   =            1.234D+02 / the double form"),
            card("PEDESTAL=                 -0.0"),
        ]);
        assert_eq!(kws[0].value(), "3.0E+02");
        assert_eq!(kws[1].value(), "1.234D+02");
        assert_eq!(kws[2].value(), "-0.0");
    }

    #[test]
    fn the_card_cap_trips() {
        let cards = vec![card("BITPIX  =                   16"); 8];
        let err = fold_cards(&region(&cards), KeywordOrigin::Image, 4, 1 << 20)
            .expect_err("the cap is exceeded");
        assert!(matches!(err, Error::LimitExceeded(_)), "got {err:?}");

        // Nine cards counting `END`, so a cap of nine is not exceeded.
        assert_eq!(
            fold_cards(&region(&cards), KeywordOrigin::Image, 9, 1 << 20)
                .expect("the cap is not exceeded")
                .len(),
            8
        );
    }

    #[test]
    fn a_region_without_an_end_card_is_malformed() {
        let mut bytes: Vec<u8> = card("BITPIX  =                   16");
        bytes.resize(BLOCK, b' ');
        assert!(matches!(
            fold_cards(&bytes, KeywordOrigin::Image, 1000, 1 << 20),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn hostile_lengths_error_rather_than_panic() {
        // Every prefix of a region parses or errors; none panics. The prefixes too short
        // to hold the `END` card at index 1 must error.
        for length in 0..=BLOCK {
            let mut bytes = region(&[card("BITPIX  =                   16")]);
            bytes.truncate(length);
            let result = fold_cards(&bytes, KeywordOrigin::Image, 1000, 1 << 20);
            if length < 2 * CARD {
                assert!(result.is_err(), "length {length} should not parse");
            } else {
                // A truncated final card is not a card, so the `END` before it stands.
                assert_eq!(
                    result.expect("the END card is still present").len(),
                    1,
                    "length {length}"
                );
            }
        }
    }

    #[test]
    fn an_unterminated_string_is_malformed() {
        assert!(matches!(
            fold_err(&[card("OBJECT  = 'never closed")]),
            Error::Malformed(_)
        ));
    }

    #[test]
    fn block_has_end_finds_the_card_anywhere_in_the_block() {
        let mut block = region(&[card("BITPIX  =                   16")]);
        assert!(block_has_end(&block));
        block.truncate(CARD);
        assert!(!block_has_end(&block));
        assert!(!block_has_end(&[]));
        assert!(!block_has_end(b"END"));
        assert!(block_has_end(&region(&[])));
    }

    #[test]
    fn lex_integer_accepts_the_conforming_forms_only() {
        assert_eq!(lex_integer("0"), Some(0));
        assert_eq!(lex_integer("42"), Some(42));
        assert_eq!(lex_integer("+42"), Some(42));
        assert_eq!(lex_integer("-32768"), Some(-32768));
        assert_eq!(lex_integer("9223372036854775807"), Some(i64::MAX));
        for reject in [
            "",
            "+",
            "-",
            " 42",
            "42 ",
            "4 2",
            "42.0",
            "1e3",
            "0x10",
            "T",
            "9223372036854775808",
            "-9223372036854775809",
        ] {
            assert_eq!(lex_integer(reject), None, "{reject:?} should not lex");
        }
    }

    #[test]
    fn lex_number_accepts_both_exponent_characters_and_integer_spellings() {
        assert_eq!(lex_number("300"), Some(300.0));
        assert_eq!(lex_number("-0.5"), Some(-0.5));
        assert_eq!(lex_number("+.5"), Some(0.5));
        assert_eq!(lex_number("2."), Some(2.0));
        assert_eq!(lex_number("3.0E+02"), Some(300.0));
        assert_eq!(lex_number("3.0e2"), Some(300.0));
        assert_eq!(lex_number("1.234D+02"), Some(123.4));
        assert_eq!(lex_number("1.234d+02"), Some(123.4));
        for reject in [
            "", "+", ".", "E5", "1e", "1e+", "1.0.0", "1 0", " 1", "inf", "NaN", "1_0", "T",
            "1e999",
        ] {
            assert_eq!(lex_number(reject), None, "{reject:?} should not lex");
        }
    }

    #[test]
    fn lex_number_reads_the_unsigned_64_convention_bzero() {
        // 2^63 exceeds `i64::MAX`, so only the float lexer reaches it.
        let bzero = lex_number("9223372036854775808").expect("2^63 lexes as a real value");
        assert_eq!(bzero, 9_223_372_036_854_775_808.0f64);
        assert_eq!(lex_integer("9223372036854775808"), None);
    }

    #[test]
    fn lex_logical_accepts_t_and_f_only() {
        assert_eq!(lex_logical("T"), Some(true));
        assert_eq!(lex_logical("F"), Some(false));
        assert_eq!(lex_logical("  T "), Some(true));
        for reject in ["", "t", "f", "true", "TF", "1", "0"] {
            assert_eq!(lex_logical(reject), None, "{reject:?} should not lex");
        }
    }
}
