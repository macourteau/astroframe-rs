//! XISF `FITSKeyword` elements, folded into the keyword list a caller reads (§11.6).
//!
//! Folding is a rule about the assembled keyword list rather than about card bytes, so the
//! `CONTINUE` chains of FITS 4.0 §4.2.1.2 and the `HIERARCH` convention govern this surface
//! exactly as they govern `crate::fits::cards` — whose entry points are driven by 80-byte cards
//! and expose nothing an attribute triple can be handed to. Nothing here reads an `<Image>`
//! attribute.

use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::metadata::{Keyword, KeywordOrigin, ValueKind};
use crate::xisf::cache::{Cache, memoized};
use crate::xisf::xml::Doc;

/// One `FITSKeyword` element's attributes, before folding — **borrowed from the document's
/// arena**.
///
/// Owning them cost three `String`s per element on top of the `Keyword` those three are then
/// packed into, and the allocation bound § Fuzzing sets counts every allocation, not the peak
/// — so a transient copy is as expensive as a kept one. The `Doc` outlives this walk, so
/// borrowing is free and the only copy is the packed one.
#[derive(Clone, Copy)]
pub(super) struct RawKeyword<'a> {
    /// The element this came from. Two occurrences of one node — a `<FITSKeyword>` reached
    /// directly and again through a `<Reference>`, or through many references — carry
    /// identical text, so the built `Keyword` is shared rather than rebuilt. See
    /// [`close_chain`].
    node: usize,
    name: &'a str,
    value: &'a str,
    comment: &'a str,
    origin: KeywordOrigin,
}

pub(super) fn read_keyword<'a>(
    doc: &'a Doc,
    node: usize,
    origin: KeywordOrigin,
) -> Option<RawKeyword<'a>> {
    Some(RawKeyword {
        node,
        // Names are trimmed of the space-filling the FITS standard mandates, which §11.6.1
        // quotes in full.
        name: doc.attr(node, "name")?.trim_ascii(),
        value: doc.attr(node, "value").unwrap_or(""),
        // §11.6.1 makes `comment` mandatory, so it is always present for an XISF-sourced
        // keyword; a writer omitting it is read as the empty comment it must have written.
        comment: doc.attr(node, "comment").unwrap_or(""),
        origin,
    })
}

/// An open `CONTINUE` chain — **the records it will assemble from, not the text it assembles**.
///
/// Nothing here is copied out of the document. The fold needs two things from the value while
/// the chain is open and neither of them needs the value to exist: whether it still ends in
/// `&`, which is what decides that the next record continues it, and how long it has grown,
/// which is what `Assembled keyword value` bounds. Both are counters. The text is built once,
/// in [`close_chain`], and only when no other image has built it already.
///
/// Building it as the records arrive is the shape three separate defects have taken. A
/// `Keyword` packs its texts into one allocation, so writing each continuation back into
/// `out[index]` copies the whole accumulated value and is quadratic in the chain length.
/// Seeding the accumulator when the chain *opens* copies the opening record's value — bounded
/// by `Attribute value length`, a mebibyte — and a value ending in `&` is the only condition
/// for opening a chain, so one `<FITSKeyword>` reached through 49 000 `<Reference>` elements
/// opens 49 000 of them: 3.11 GB from a one-megabyte header, against 24 MB without the
/// trailing `&`. Copying the opening record's `comment` there cost 540 MB from a 300 KB header
/// in the same way, and copying each continuation's cost the same per record.
struct Chain<'a> {
    /// Where in `out` the record that opened the chain sits.
    index: usize,
    /// The record that opened it. Its node and origin are the first half of the memo key, its
    /// `comment` is §4.2.1.2's fallback, and it is copied rather than borrowed because a
    /// `RawKeyword` is four words of arena pointers.
    opener: RawKeyword<'a>,
    /// The continuation records folded in so far, as positions in the record list.
    ///
    /// A run rather than a list: any record that does not continue the chain closes it, so
    /// the records folded into one chain are contiguous and immediately after its opener.
    continuations: std::ops::Range<usize>,
    /// The last continued record's comment, by §4.2.1.2's precedence — and **`None` until a
    /// continuation with a non-empty comment actually arrives**, which is when the opening
    /// record's stops being the answer. Borrowed from the arena, so it is a pointer and not a
    /// copy however many continuations carry one.
    comment: Option<&'a str>,
    /// What the assembled value would measure: its length in bytes, which is what
    /// `Assembled keyword value` is checked against as the chain grows, and its trailing run
    /// of `&`.
    ///
    /// The run rather than a flag, because a continuation that assembles to nothing pops one
    /// `&` off the end and leaves whatever preceded it: `'&&&'` continued by `''` still ends
    /// in `&`, and a flag reads that as a closed chain.
    len: usize,
    trailing_amps: usize,
}

/// Assemble a closed chain and write it into the keyword it started from, packing it once.
///
/// The XISF twin of `fits::cards::close_chain`, and the same reason: an assembled value is
/// written once when the chain closes, never per continuation record.
///
/// **This is where the memo belongs**, the assembled `Keyword` being the expensive object.
/// [`Cache`] shares the individual records a chain is built from, and that covers everything
/// [`fold_records`] allocates *except* this: a chain's value is built from several records at
/// once, so no per-record memo can hold it, and the fold runs once per distinct `<Image>` —
/// `walk_occurrences`'s own memo covers reference-reached images only. So 256 distinct images
/// each carrying `<Reference ref="k0"/><Reference ref="k1"/>` to one half-megabyte opener and
/// one half-megabyte continuation each assembled their own megabyte-long `String`: 1.03 GB
/// allocated and 264 MB held live from a 1.05 MB header, 982× its input, with every count
/// inside its cap. `Assembled keyword value` times `Images per source` is a product no part of
/// the input relates to.
///
/// The key is **exactly what the assembled `Keyword` is a function of, and no more**: the
/// opening record's node and reported origin, then each continuation's node in order. The
/// value is the opener's text with each continuation's appended, the comment is the opener's
/// or the last non-empty continuation's, and the name and origin are the opener's — a
/// record's text being its element's attributes, which its node fixes, and a continuation
/// contributing no origin because it is folded away rather than reported.
///
/// **A key over anything wider is defeated by whatever it added.** This memo was first written
/// over the whole image's record sequence, on the reasoning that the folded *list* is a
/// function of that sequence — true, and beside the point: one distinguishing `<FITSKeyword>`
/// per image is outside the assembly's inputs and inside that key, so two images stopped
/// sharing a chain that is byte-identical in both, and the same 256 images went from 11.3×
/// their input to 982×. `docs/intentional-patterns.md` states the general rule.
///
/// **Consulted only for a reference-reached opener**, which is [`Cache`]'s own gate and the
/// same argument: a `<FITSKeyword>` attached directly to an image is that image's child and
/// belongs to no other, so its chain is assembled once and a memo over it is pure overhead.
/// The gate therefore fixes the origin in every key it ever builds. The origin stays in the
/// key regardless, because it is an input the assembled `Keyword` genuinely has — a key that
/// is complete only while the gate holds is a key that breaks silently when the gate moves.
fn close_chain(out: &mut [Keyword], state: &Chain<'_>, raw: &[RawKeyword<'_>], cache: &mut Cache) {
    // Nothing to assemble when no continuation arrived: the keyword already holds its own
    // value, and rebuilding it would allocate a fresh buffer identical to the shared one it
    // has.
    if state.continuations.is_empty() {
        return;
    }
    let Some(opener) = out.get(state.index) else {
        return;
    };
    let records = &raw[state.continuations.clone()];
    let mut nodes = Vec::with_capacity(records.len() + 1);
    nodes.push(state.opener.node);
    nodes.extend(records.iter().map(|record| record.node));
    let assembled = memoized(
        &mut cache.chains,
        (state.opener.origin, nodes),
        state.opener.origin == KeywordOrigin::Reference,
        || {
            let mut value = String::with_capacity(state.len);
            value.push_str(opener.value());
            for record in records {
                value.pop(); // the `&` this record continues from
                // Scanned once already, to decide that it continued the chain at all; the
                // copy is what was deferred to here.
                value.push_str(&unquote(record.value).expect("a folded record is a quoted value"));
            }
            // §4.2.1.2's precedence, resolved here rather than carried: the last continued
            // record's comment when it had one, and the opening record's otherwise.
            let comment = state.comment.unwrap_or(state.opener.comment);
            // A chain assembles character strings and nothing else, so the assembled
            // keyword's kind is the opening record's.
            Keyword::new(
                opener.name(),
                &value,
                Some(comment),
                opener.origin(),
                opener.value_kind(),
            )
        },
    );
    out[state.index] = assembled;
}

/// Fold the assembled keyword list.
///
/// Folding is a rule about the assembled list rather than about card bytes, so it applies to
/// this surface too: nothing in XISF forbids a writer from serializing either convention,
/// §11.6 asks for FITS-transparent access, and the FITS version it names predates `CONTINUE`'s
/// standardization, which makes improvised spellings *more* likely here. Folding on one surface
/// only would make the same long string read differently through the two containers.
///
/// Both edge cases of FITS 4.0 §4.2.1.2 hold: a value ending in `&` with no conforming
/// `CONTINUE` after it keeps the `&`, and an orphaned `CONTINUE` is commentary text.
///
/// The equivalent logic in `crate::fits::cards` is driven by 80-byte card bytes and exposes no
/// entry point over an assembled list, so it is reproduced here rather than reshaped.
///
/// One `<FITSKeyword>` reached through N `<Reference>` elements is N occurrences of the same
/// element, and § XISF decisions requires each to be reported. Building each from the text made
/// a header of 49 000 references to one 21 KB keyword — a megabyte of input, inside every cap —
/// allocate a gigabyte and retain it. Every record here goes through [`Cache`] instead: the same
/// node is the same text, and `Keyword`'s buffer is an `Arc`, so a repeat costs a refcount.
/// **Both** branches do — an orphaned `CONTINUE` is as retained as any other keyword. The one
/// thing a per-record memo cannot hold is a chain's assembly, which [`close_chain`] holds
/// instead.
pub(super) fn fold_records<'a>(
    raw: &[RawKeyword<'a>],
    cache: &mut Cache,
    limits: &Limits,
) -> Result<Vec<Keyword>> {
    let mut out: Vec<Keyword> = Vec::new();
    let mut chain: Option<Chain<'a>> = None;

    for (position, record) in raw.iter().enumerate() {
        let referenced = record.origin == KeywordOrigin::Reference;
        if record.name == "CONTINUE" {
            // An un-continued chain has nothing appended to it yet; it was opened because the
            // keyword's own value ends in `&`, which is the condition, so it is continuable.
            // Checked first, so that a record which cannot continue anything does no work on
            // the value it would have continued.
            let open = chain
                .as_ref()
                .is_some_and(|state| state.continuations.is_empty() || state.trailing_amps > 0);
            // `quoted_content`, not `unquote`: this decides whether the record continues the
            // chain and how the assembled value's tail moves, and neither needs the text to
            // exist. Unquoting here copied a mebibyte-capped attribute per continuation per
            // image, on a record that is then folded away rather than reported.
            let continued = match open {
                true => quoted_content(record.value),
                false => None,
            };
            match continued {
                Some(content) => {
                    let state = chain.as_mut().expect("an open chain");
                    let (added, trailing) = unquoted_shape(content);
                    // One `&` is popped off the accumulated value, then this record's text is
                    // appended -- measured rather than performed, `close_chain` doing the
                    // building once for every image that assembles the same records.
                    state.len = state.len - 1 + added;
                    state.trailing_amps = match added {
                        // Nothing appended: the pop is all that happened, and whatever `&`
                        // preceded it is still the tail.
                        0 => state.trailing_amps - 1,
                        // Nothing but `&`: this record's run joins what the pop left behind.
                        _ if trailing == added => state.trailing_amps - 1 + added,
                        _ => trailing,
                    };
                    state.continuations.end = position + 1;
                    // Checked as the value grows, not once at the end: a chain is the one
                    // place a *reported* value's length is bounded by how it was reached
                    // rather than by how it was written, because a `<Reference>` can reach one
                    // continuation record many times. Refusing here keeps the buffer bounded;
                    // refusing after assembly would already have allocated it.
                    if state.len as u64 > limits.keyword_value_bytes {
                        return Err(Error::limit(format!(
                            "assembled keyword value: a CONTINUE chain assembles more than \
                             the {} bytes the cap allows",
                            limits.keyword_value_bytes
                        )));
                    }
                    // §4.2.1.2 leaves the comment of an assembled keyword open. The last
                    // continued record's comment wins when it has one and the first record's
                    // otherwise; `comment` being mandatory here, "has one" means non-empty
                    // rather than present. Borrowed either way -- `close_chain` resolves the
                    // fallback -- so no comment is copied for a chain nobody assembles.
                    if !record.comment.is_empty() {
                        state.comment = Some(record.comment);
                    }
                    if state.trailing_amps == 0 {
                        let closed = chain.take().expect("just borrowed");
                        close_chain(&mut out, &closed, raw, cache);
                    }
                }
                None => {
                    // An orphaned record is commentary text. It is also where a non-conforming
                    // one lands, which is why the value it failed to continue keeps its `&` —
                    // nothing above touched it. The two attributes are rejoined the way a card
                    // body writes them, so no text is lost to the commentary shape.
                    //
                    // Memoized like every other record, and for the same reason: the rejoined
                    // body is a copy of two attributes that the `Attribute value length` cap
                    // lets be a mebibyte each, and it is **retained** — 2048 references to one
                    // orphaned `CONTINUE` allocated 6.4 GB from a one-megabyte header. A node
                    // named `CONTINUE` is named that on every occurrence, so one memo serves
                    // both branches of this function.
                    out.push(memoized(
                        &mut cache.keywords,
                        record.node,
                        referenced,
                        || {
                            let body = join_body(record.value, record.comment);
                            Keyword::new(
                                "CONTINUE",
                                "",
                                Some(&body),
                                record.origin,
                                ValueKind::Commentary,
                            )
                        },
                    ));
                    if let Some(closed) = chain.take() {
                        close_chain(&mut out, &closed, raw, cache);
                    }
                }
            }
            continue;
        }

        // Any other keyword breaks the chain: §4.2.1.2 continues an *immediately* following
        // record. A chain broken this way is still assembled from what it accumulated.
        if let Some(closed) = chain.take() {
            close_chain(&mut out, &closed, raw, cache);
        }

        // Looked up **before** the unquoting below, which is the allocation being avoided: it
        // copies the value text, and copying it 49 000 times is the gigabyte.
        let keyword = memoized(&mut cache.keywords, record.node, referenced, || {
            let (name, value_text) = hierarch(record.name, record.value);
            // XISF stores FITS keywords with their FITS quoting intact — the specification's
            // own example is `value="'2012-03-15T02:55:15'"` — so the unquoting rule applies
            // here exactly as it does to a card, and so does the value kind it settles.
            let (value, kind) = match unquote(value_text) {
                Some(text) => (text, ValueKind::CharacterString),
                None => (value_text.trim_ascii().to_owned(), ValueKind::Other),
            };
            Keyword::new(&name, &value, Some(record.comment), record.origin, kind)
        });

        // §4.2.1.2 continues character strings only, so a numeric value ending in `&` is a
        // value ending in `&`. Read off the built keyword rather than carried beside it: the
        // kind a memo hit needs is on the value it hit.
        if keyword.value_kind() == ValueKind::CharacterString && keyword.value().ends_with('&') {
            let value = keyword.value();
            chain = Some(Chain {
                index: out.len(),
                opener: *record,
                continuations: position + 1..position + 1,
                comment: None,
                len: value.len(),
                trailing_amps: value.len() - value.trim_end_matches('&').len(),
            });
        }
        out.push(keyword);
    }

    // A chain still open when the records run out is assembled from what it accumulated --
    // the same answer `fits::cards::fold_cards` gives, and the reason it must: a continuation
    // that is folded into the opening record is not also pushed on its own, so dropping the
    // chain here drops the record's text with it. Silently. That is the one outcome § The
    // organizing principle rules out, and it made the same keyword read differently through
    // the two containers, which *A keyword reads the same from either container* forbids.
    if let Some(closed) = chain.take() {
        close_chain(&mut out, &closed, raw, cache);
    }
    Ok(out)
}

/// The multi-word name a `HIERARCH` record carries, in either of the two spellings an XISF
/// writer can reach for.
///
/// `HIERARCH` appears nowhere in FITS 4.0; it is an ESO convention that carries the name in the
/// card body, so a writer serializing one into §11.6.1's three attributes may put the whole
/// name in `name` or the whole card body in `value`. Both resolve to the full multi-word name,
/// never the bare `HIERARCH`.
fn hierarch<'a>(name: &'a str, value: &'a str) -> (String, &'a str) {
    if let Some(rest) = name.strip_prefix("HIERARCH")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        let collapsed = collapse_whitespace(rest);
        if !collapsed.is_empty() {
            return (collapsed, value);
        }
    }
    if name == "HIERARCH"
        && let Some((multi_word, rest)) = value.split_once('=')
        // A quote before the value indicator makes this a value rather than a name:
        // `HIERARCH = 'x=y'` names nothing and is an ordinary keyword called `HIERARCH`.
        && !multi_word.contains('\'')
    {
        let collapsed = collapse_whitespace(multi_word);
        if !collapsed.is_empty() {
            return (collapsed, rest);
        }
    }
    (name.to_owned(), value)
}

/// A FITS character-string value's content (§4.2.1) **as the source spells it**: the text
/// between the opening quote and the closing one, doubled quotes still doubled and trailing
/// blanks still there. `None` if the text does not open with a quote or never closes one.
///
/// Split out of [`unquote`] because the fold has to decide whether a `CONTINUE` record
/// continues the chain before it decides whether the chain is worth assembling at all, and
/// that decision reads the tail rather than the text. Scanning is free; the copy is the cost,
/// and this is the half that does not copy.
fn quoted_content(text: &str) -> Option<&str> {
    let body = text.trim_ascii_start().strip_prefix('\'')?;
    let mut from = 0;
    loop {
        let quote = from + body[from..].find('\'')?;
        // A doubled quote is one literal quote rather than the end of the value.
        match body[quote + 1..].starts_with('\'') {
            true => from = quote + 2,
            false => return Some(&body[..quote]),
        }
    }
}

/// What [`unquote`] would return, measured rather than built: its length in bytes and its
/// trailing run of `&`.
///
/// Both read off the source text directly. Collapsing `''` to `'` shortens the content by one
/// byte a pair and introduces neither a blank nor an `&`, so it moves the length by the pair
/// count and cannot move either trim boundary.
fn unquoted_shape(content: &str) -> (usize, usize) {
    // Trailing blanks are not significant; leading blanks are.
    let trimmed = content.trim_ascii_end();
    let len = trimmed.len() - trimmed.matches("''").count();
    (len, trimmed.len() - trimmed.trim_end_matches('&').len())
}

/// Unquote a FITS character-string value (§4.2.1). `None` if the text does not open with a
/// quote or never closes one.
fn unquote(text: &str) -> Option<String> {
    let mut rest = quoted_content(text)?.trim_ascii_end();
    let mut content = String::with_capacity(rest.len());
    while let Some(pair) = rest.find("''") {
        content.push_str(&rest[..pair + 1]);
        rest = &rest[pair + 2..];
    }
    content.push_str(rest);
    Some(content)
}

/// Rejoin a record's value and comment the way a card body writes them.
fn join_body(value: &str, comment: &str) -> String {
    let value = value.trim_ascii_end();
    match (value.is_empty(), comment.is_empty()) {
        (true, _) => comment.to_owned(),
        (false, true) => value.to_owned(),
        (false, false) => format!("{value} / {comment}"),
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use crate::xisf::image::tests::one;

    #[test]
    fn xisf_side_fits_quoting_is_unwrapped() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <FITSKeyword name="DATE-OBS  " value="'2012-03-15T02:55:15'"
                                comment="Observation start time, UT"/>
                   <FITSKeyword name="EXPTIME" value="300" comment="Exposure time in seconds"/>
                   <FITSKeyword name="OBJECT" value="'  M 31   '" comment=""/>
                   <FITSKeyword name="QUOTED" value="'it''s here'" comment=""/>
                   <FITSKeyword name="HISTORY" value="" comment="Processed with magic"/>
                 </Image>
               </xisf>"#);
        let keywords = o.header.keywords();
        assert_eq!(keywords[0].name(), "DATE-OBS", "names are trimmed");
        assert_eq!(keywords[0].value(), "2012-03-15T02:55:15");
        assert_eq!(keywords[0].comment(), Some("Observation start time, UT"));
        assert_eq!(keywords[1].value(), "300");
        // Trailing blanks are stripped and leading ones are kept.
        assert_eq!(keywords[2].value(), "  M 31");
        assert_eq!(keywords[3].value(), "it's here");
        // HISTORY carries an empty value by specification and its text lives in the comment.
        assert_eq!(keywords[4].value(), "");
        assert_eq!(keywords[4].comment(), Some("Processed with magic"));
        // `comment` is mandatory in §11.6.1, so it is always present here.
        assert!(keywords.iter().all(|k| k.comment().is_some()));
    }

    #[test]
    fn a_continue_chain_assembles_and_both_edge_cases_hold() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <FITSKeyword name="LONGSTR" value="'a very long string va&amp;'" comment="one"/>
                   <FITSKeyword name="CONTINUE" value="'lue that spans cards'" comment=""/>
                   <FITSKeyword name="DANGLE" value="'ends in an ampersand&amp;'" comment=""/>
                   <FITSKeyword name="BREAK" value="1" comment=""/>
                   <FITSKeyword name="CONTINUE" value="'orphaned text'" comment="stray"/>
                 </Image>
               </xisf>"#);
        let keywords = o.header.keywords();
        assert_eq!(keywords[0].name(), "LONGSTR");
        assert_eq!(
            keywords[0].value(),
            "a very long string value that spans cards"
        );
        // A chain that comments only its opening record keeps that comment.
        assert_eq!(keywords[0].comment(), Some("one"));
        // Edge case 1: a value ending in `&` with no conforming CONTINUE after it keeps it.
        assert_eq!(keywords[1].value(), "ends in an ampersand&");
        assert_eq!(keywords[2].name(), "BREAK");
        // Edge case 2: an orphaned CONTINUE is commentary text, and loses none of it.
        assert_eq!(keywords[3].name(), "CONTINUE");
        assert_eq!(keywords[3].value(), "");
        assert_eq!(keywords[3].comment(), Some("'orphaned text' / stray"));
    }

    #[test]
    fn hierarch_records_carry_their_full_names_in_either_spelling() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <FITSKeyword name="HIERARCH ESO DET EXP" value="12" comment="a"/>
                   <FITSKeyword name="HIERARCH" value="ESO DET CHIP = 'CCD1'" comment="b"/>
                   <FITSKeyword name="HIERARCH" value="'x=y'" comment="c"/>
                 </Image>
               </xisf>"#);
        let keywords = o.header.keywords();
        assert_eq!(keywords[0].name(), "ESO DET EXP");
        assert_eq!(keywords[0].value(), "12");
        assert_eq!(keywords[1].name(), "ESO DET CHIP");
        assert_eq!(keywords[1].value(), "CCD1");
        // `HIERARCH = 'x=y'` names nothing and stays an ordinary keyword.
        assert_eq!(keywords[2].name(), "HIERARCH");
        assert_eq!(keywords[2].value(), "x=y");
    }
}
