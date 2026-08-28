//! *Every FITS decision in § FITS decisions has a test.*
//!
//! Individually small, collectively the largest untested surface on the FITS side — which is
//! why the criterion names them one by one rather than leaving "the decisions are covered" to
//! a reader's judgement.

mod common;

use astroframe::{Bounds, KeywordOrigin, Reader, RowOrder, Samples, Scaling};
use common::Hdu;
use std::io::Cursor;

/// A primary that holds no image, carrying the four cards `INHERIT` can gate plus two it
/// cannot.
fn primary_with_inheritable_cards() -> Hdu {
    Hdu::primary()
        .card("BITPIX", "8")
        .card("NAXIS", "0")
        .card("BSCALE", "1")
        .card("BZERO", "32768")
        .card("BLANK", "-32768")
        .card("ROWORDER", "'BOTTOM-UP'")
        .card("EXPTIME", "120.0")
}

/// An `IMAGE` extension over four `i16` samples, with the mandatory extension keywords.
fn image_extension(cards: &[(&str, &str)], samples: &[i16]) -> Hdu {
    let mut hdu = Hdu::extension("IMAGE")
        .image_2d(16, 4, 1)
        .card("PCOUNT", "0")
        .card("GCOUNT", "1");
    for (name, value) in cards {
        hdu = hdu.card(name, value);
    }
    hdu.data_i16(samples)
}

fn advance_to_extension(bytes: Vec<u8>) -> Reader<astroframe::Seekable<Cursor<Vec<u8>>>> {
    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    assert!(
        reader.next_image().expect("advance"),
        "expected an image position"
    );
    reader
}

/// **`INHERIT` gates *application*, never reporting, and only of the four cards that change
/// what a pixel means.** The extension's own card always wins; inheritance fills gaps and
/// never overrides; and **the test is per card**, so a mixed pairing is reachable and
/// legitimate.
///
/// Getting this wrong is not a visible error — applying a primary's `BSCALE` over an
/// extension's own rewrites every pixel and moves the frame between "unsigned convention" and
/// "no normalized output". That is the silent plausible repair this design refuses everywhere.
#[test]
fn inherit_fills_gaps_per_card_and_never_overrides() {
    // The extension carries both, under INHERIT = T: its own values stand.
    let bytes = common::file(&[
        primary_with_inheritable_cards(),
        image_extension(
            &[("INHERIT", "T"), ("BSCALE", "1"), ("BZERO", "0")],
            &[0, 1, 2, 3],
        ),
    ]);
    let reader = advance_to_extension(bytes);
    let header = reader.header().expect("header");
    assert_eq!(
        header.scaling(),
        Some(Scaling::Fits {
            bscale: 1.0,
            bzero: 0.0
        }),
        "the extension's own BZERO must win over the primary's"
    );

    // The extension carries BSCALE but not BZERO, under INHERIT = T: its own BSCALE beside
    // the primary's BZERO. This is the mixed pairing, and it is the case a per-header
    // implementation gets wrong while passing both of the others.
    let bytes = common::file(&[
        primary_with_inheritable_cards(),
        image_extension(&[("INHERIT", "T"), ("BSCALE", "1")], &[0, 1, 2, 3]),
    ]);
    let reader = advance_to_extension(bytes);
    assert_eq!(
        reader.header().expect("header").scaling(),
        Some(Scaling::Fits {
            bscale: 1.0,
            bzero: 32768.0
        }),
        "BZERO must be inherited to fill the gap the extension leaves"
    );
}

/// Under `INHERIT = F`, **and equally when the extension carries no `INHERIT` card at all**,
/// no primary card is applied: the extension's own values stand, and where it carries none the
/// FITS defaults do.
#[test]
fn inherit_false_and_inherit_absent_both_apply_nothing() {
    for cards in [vec![("INHERIT", "F")], vec![]] {
        let bytes = common::file(&[
            primary_with_inheritable_cards(),
            image_extension(&cards, &[0, 1, 2, 3]),
        ]);
        let reader = advance_to_extension(bytes);
        let header = reader.header().expect("header");
        assert_eq!(
            header.scaling(),
            Some(Scaling::Fits {
                bscale: 1.0,
                bzero: 0.0
            }),
            "with INHERIT {cards:?} the FITS defaults stand, not the primary's BZERO"
        );
        // ROWORDER is one of the four gated cards, so it is not inherited either.
        assert_eq!(header.row_order(), Some(&RowOrder::Unspecified));
    }
}

/// **`INHERIT = T` applies the primary's `ROWORDER` to an extension that carries none of its
/// own.** `ROWORDER` is resolved beside `applied` rather than through it (see `INHERITABLE` in
/// `src/fits/decoder.rs`), so it needs its own positive coverage: the shared per-card gap-fill
/// test above only exercises `BSCALE`/`BZERO`, and the negative `INHERIT = F` / absent case does
/// not exercise inheritance actually firing.
#[test]
fn inherit_true_applies_the_primarys_roworder() {
    let bytes = common::file(&[
        primary_with_inheritable_cards(),
        image_extension(&[("INHERIT", "T")], &[0, 1, 2, 3]),
    ]);
    let reader = advance_to_extension(bytes);
    let header = reader.header().expect("header");
    assert_eq!(
        header.row_order(),
        Some(&RowOrder::BottomUp),
        "the extension carries no ROWORDER of its own, so the primary's BOTTOM-UP applies"
    );
}

/// **Reporting is never gated on `INHERIT`.** Real archive frames put `DATE-OBS`, `EXPTIME`
/// and `EGAIN` in the primary and frequently omit the keyword; gating the report would lose a
/// consumer its entire keyword list on exactly those files.
#[test]
fn both_headers_cards_are_always_reported_and_tagged() {
    let bytes = common::file(&[
        primary_with_inheritable_cards(),
        image_extension(&[("INHERIT", "F")], &[0, 1, 2, 3]),
    ]);
    let reader = advance_to_extension(bytes);
    let header = reader.header().expect("header");

    let exptime = header
        .get("EXPTIME")
        .expect("the primary's EXPTIME is reported");
    assert_eq!(exptime.value(), "120.0");
    assert_eq!(exptime.origin(), KeywordOrigin::PrimaryHeader);

    // The extension's own card wins a lookup, because lookup returns the first match in
    // stored order and the extension's cards are stored first.
    let bitpix = header.get("BITPIX").expect("BITPIX");
    assert_eq!(bitpix.origin(), KeywordOrigin::Image);
    assert_eq!(bitpix.value(), "16");

    // Both occurrences stay reachable through the list.
    let all: Vec<_> = header
        .keywords()
        .iter()
        .filter(|k| k.name() == "BITPIX")
        .collect();
    assert_eq!(all.len(), 2, "both headers' BITPIX cards are reported");
}

/// An `INHERIT` card in a **primary** header gates nothing — Appendix K forbids it there, so a
/// card found there is data rather than an instruction — and is reported like any other.
#[test]
fn inherit_in_a_primary_header_is_data_not_an_instruction() {
    let bytes = common::file(&[Hdu::primary()
        .image_2d(16, 4, 1)
        .unsigned_convention(16)
        .card("INHERIT", "T")
        .data_i16(&[-32768, -32767, 0, 32767])]);
    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let header = reader.header().expect("header");
    assert_eq!(header.get("INHERIT").map(|k| k.value()), Some("T"));
    // It gated nothing: the primary's own scaling stands and the frame still decodes.
    assert_eq!(
        header.scaling(),
        Some(Scaling::Fits {
            bscale: 1.0,
            bzero: 32768.0
        })
    );
    assert!(matches!(
        header.bounds(),
        Bounds::FormatDefault(range) if range.lo() == 0.0 && range.hi() == 65535.0
    ));
}

/// **`BLANK` is reported and no sample is substituted.**
///
/// Deliberately *not* symmetric with NaN preservation: a NaN survives normalization intact,
/// whereas a `BLANK` integer normalizes to an ordinary value and — at the widths where the map
/// is lossy — cannot be recovered from the normalized output. A consumer needing blank masking
/// reads the keyword and applies it against native samples, the only place the information is
/// still exact.
#[test]
fn blank_is_reported_and_no_sample_is_substituted() {
    let stored: [i16; 4] = [-32768, -32767, 0, 32767];
    let bytes = common::file(&[Hdu::primary()
        .image_2d(16, 4, 1)
        .unsigned_convention(16)
        .card("BLANK", "-32768")
        .data_i16(&stored)]);
    let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let header = reader.header().expect("header");
    assert_eq!(header.get("BLANK").map(|k| k.value()), Some("-32768"));

    let mut samples = Samples::zeroed(header.sample_format().expect("format"), 4);
    reader
        .read_samples_into(&mut samples)
        .expect("native decode");
    assert_eq!(
        samples,
        Samples::I16(stored.to_vec()),
        "the blank value is stored, not masked"
    );

    let image = advance_and_decode(&bytes);
    // Level 0 under the unsigned convention: an ordinary value, not a sentinel.
    assert_eq!(image[0].to_bits(), 0x0000_0000);
    assert_eq!(image[3].to_bits(), 1.0f32.to_bits());
}

/// **`CHECKSUM`/`DATASUM` are reported and not verified.** Unlike XISF's block checksums, FITS
/// makes these advisory rather than mandatory-when-present — so a wrong one must not fail the
/// frame.
#[test]
fn fits_checksum_keywords_are_reported_and_not_verified() {
    let bytes = common::file(&[Hdu::primary()
        .image_2d(16, 4, 1)
        .unsigned_convention(16)
        .card("CHECKSUM", "'0000000000000000'")
        .card("DATASUM", "'99999999'")
        .data_i16(&[-32768, -32767, 0, 32767])]);
    let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let header = reader.header().expect("header");
    assert_eq!(
        header.get("CHECKSUM").map(|k| k.value()),
        Some("0000000000000000")
    );
    assert_eq!(header.get("DATASUM").map(|k| k.value()), Some("99999999"));
    // A deliberately wrong pair; the frame decodes anyway.
    let image = advance_and_decode(&bytes);
    assert_eq!(image[0].to_bits(), 0x0000_0000);
}

/// **`ROWORDER` is read into `{TopDown, BottomUp, Unspecified, Other}` and applied to
/// nothing.** Recognition is case-insensitive and trims surrounding whitespace.
///
/// The spelling normalization is a correctness rule rather than a convenience: it is what
/// stops a consumer's envelope predicate from admitting a flipped frame through a spelling
/// variant. `ROWORDER` is a convention and appears nowhere in FITS 4.0, so unrecognized values
/// are expected and are reported verbatim.
#[test]
fn roworder_spellings_normalize_and_unknown_values_survive_verbatim() {
    let cases: [(&str, RowOrder); 6] = [
        ("'TOP-DOWN'", RowOrder::TopDown),
        ("'top-down'", RowOrder::TopDown),
        ("'  BOTTOM-UP  '", RowOrder::BottomUp),
        ("'Bottom-Up'", RowOrder::BottomUp),
        ("'SPIRAL'", RowOrder::Other("SPIRAL".into())),
        ("'  '", RowOrder::Other("".into())),
    ];
    for (written, expected) in cases {
        let bytes = common::file(&[Hdu::primary()
            .image_2d(16, 4, 1)
            .unsigned_convention(16)
            .card("ROWORDER", written)
            .data_i16(&[-32768, -32767, 0, 32767])]);
        let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
        assert!(reader.next_image().expect("advance"));
        assert_eq!(
            reader.header().expect("header").row_order(),
            Some(&expected),
            "ROWORDER = {written}"
        );
    }

    // Absence is a different fact from a declared value: the convention is one a file may
    // simply not have invoked.
    let bytes = common::file(&[Hdu::primary()
        .image_2d(16, 4, 1)
        .unsigned_convention(16)
        .data_i16(&[-32768, -32767, 0, 32767])]);
    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    assert!(reader.next_image().expect("advance"));
    assert_eq!(
        reader.header().expect("header").row_order(),
        Some(&RowOrder::Unspecified)
    );
}

/// **FITS pixel data is big-endian two's-complement, always.** The format carries no attribute
/// to read and no default to infer, so it is pinned for the same reason XISF's little-endian
/// default is: getting it wrong corrupts every sample silently rather than raising an error.
///
/// The samples are chosen so a little-endian read gives different values rather than the same
/// ones — a palindromic pattern would pass either way.
#[test]
fn fits_samples_are_big_endian() {
    let stored: [i16; 4] = [0x0102, 0x7F01, -2, 0x0080];
    let bytes = common::file(&[Hdu::primary()
        .image_2d(16, 4, 1)
        .card("BSCALE", "1")
        .card("BZERO", "0")
        .data_i16(&stored)]);
    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let mut samples = Samples::zeroed(astroframe::SampleFormat::I16, 4);
    reader
        .read_samples_into(&mut samples)
        .expect("native decode");
    assert_eq!(samples, Samples::I16(stored.to_vec()));
    for s in stored {
        assert_ne!(
            s,
            s.swap_bytes(),
            "a palindromic sample would pass either way"
        );
    }
}

/// **`NAXIS = 3` is read as channels**, under the same caps as everything else.
///
/// A spectral or temporal cube is structurally indistinguishable from a colour image in FITS,
/// so this is deliberately more permissive than the XISF side, where higher-dimensional
/// geometry is `Unsupported` outright: XISF states its dimensionality explicitly and can be
/// refused precisely, whereas refusing all FITS `NAXIS = 3` would reject ordinary RGB frames.
#[test]
fn naxis_three_is_read_as_channels() {
    let stored: Vec<i16> = (0..12).map(|i| (i - 32768) as i16).collect();
    let bytes = common::file(&[Hdu::primary()
        .image_3d(16, 2, 2, 3)
        .unsigned_convention(16)
        .data_i16(&stored)]);
    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let header = reader.header().expect("header");
    assert_eq!(
        (header.width(), header.height(), header.channels()),
        (Some(2), Some(2), Some(3))
    );
    let image = reader.read_image().expect("decode");
    assert_eq!(image.samples().len(), 12);
    assert_eq!(image.channel(2).expect("third channel").len(), 4);
}

fn advance_and_decode(bytes: &[u8]) -> Vec<f32> {
    let mut reader = Reader::seekable(Cursor::new(bytes.to_vec())).expect("construct");
    assert!(reader.next_image().expect("advance"));
    reader.read_image().expect("decode").into_samples()
}

/// **The data unit is sized `|BITPIX|/8 × GCOUNT × (PCOUNT + Π NAXISᵢ)`, not `BITPIX × NAXIS*`.**
///
/// The naive form lands mid-file on any heap-carrying `BINTABLE` and misparses everything
/// after it, silently: the walk resumes inside the heap, where any 2880-byte block that
/// happens to start with `XTENSION` is read as a header. The fixtures make each term
/// load-bearing on its own by sizing the payload so that dropping the term moves the next
/// header by a whole block rather than by a remainder the padding would absorb.
#[test]
fn a_data_unit_is_sized_with_pcount_and_gcount() {
    /// A 4-sample `i16` image extension whose pixel values identify it.
    fn marker() -> Hdu {
        image_extension(&[("BZERO", "32768"), ("BSCALE", "1")], &[1, 2, 3, 4])
    }

    // PCOUNT is the heap. Table data is one block, heap is one more: reading PCOUNT as zero
    // stops the skip a block early, on the heap rather than on the marker's header.
    let heap = Hdu::extension("BINTABLE")
        .card("BITPIX", "8")
        .card("NAXIS", "2")
        .card("NAXIS1", "2880")
        .card("NAXIS2", "1")
        .card("PCOUNT", "2880")
        .card("GCOUNT", "1")
        // Binary, as table and heap bytes are: a wrongly-sized step lands the card lexer on a
        // byte outside 0x20-0x7E rather than on filler it could read as commentary.
        .data((0..5760u32).map(|i| (i % 251) as u8).collect());
    let reader = advance_to_extension(common::file(&[
        Hdu::primary().card("BITPIX", "8").card("NAXIS", "0"),
        heap,
        marker(),
    ]));
    let header = reader.header().expect("the marker's header");
    assert_eq!(
        (header.width(), header.height()),
        (Some(4), Some(1)),
        "the walk resumed on the marker rather than inside the heap"
    );

    // GCOUNT is the group repeat, and the same argument applies to it.
    let grouped = Hdu::extension("BINTABLE")
        .card("BITPIX", "8")
        .card("NAXIS", "2")
        .card("NAXIS1", "2880")
        .card("NAXIS2", "1")
        .card("PCOUNT", "0")
        .card("GCOUNT", "2")
        .data((0..5760u32).map(|i| (i % 251) as u8).collect());
    let reader = advance_to_extension(common::file(&[
        Hdu::primary().card("BITPIX", "8").card("NAXIS", "0"),
        grouped,
        marker(),
    ]));
    let header = reader.header().expect("the marker's header");
    assert_eq!(
        (header.width(), header.height()),
        (Some(4), Some(1)),
        "the walk resumed on the marker rather than inside the group data"
    );
}

/// **A declined position is still stepped over exactly**, and random groups is the case where
/// that costs something: §6.1.1 fixes `NAXIS1 = 0`, so the axis product that sizes every other
/// HDU is zero here and the primary's otherwise-implied `PCOUNT = 0`, `GCOUNT = 1` do not hold.
/// Sizing it the ordinary way makes the data unit zero bytes and resumes the walk inside the
/// group data.
///
/// `GROUPS = T` is `Unsupported` at a declined position (§ the decline table), so the caller
/// sees the decline **and** can walk past it to the image that follows.
#[test]
fn a_random_groups_primary_is_declined_and_stepped_over_exactly() {
    // Per group: 4 parameters + 1438 data values, at 2 bytes each = 2884 bytes; two groups is
    // 5768, padded to three blocks. Read as an ordinary primary it sizes to zero, because
    // NAXIS1 = 0 takes the axis product with it.
    let groups = Hdu::primary()
        .card("BITPIX", "16")
        .card("NAXIS", "2")
        .card("NAXIS1", "0")
        .card("NAXIS2", "1438")
        .card("GROUPS", "T")
        .card("PCOUNT", "4")
        .card("GCOUNT", "2")
        // Binary group data, not filler that could pass for header text: a byte outside
        // 0x20-0x7E is exactly what a wrongly-sized step would land the card lexer on.
        .data((0..5768u32).map(|i| (i % 251) as u8).collect());
    let bytes = common::file(&[
        groups,
        image_extension(&[("BZERO", "32768"), ("BSCALE", "1")], &[1, 2, 3, 4]),
    ]);

    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    assert!(
        reader.next_image().expect("advance"),
        "the random-groups primary is an image position, declined rather than skipped"
    );
    let header = reader
        .header()
        .expect("a declined position still reports what it can");
    let reason = header
        .decline_reason()
        .expect("the random-groups primary is a declined position");
    assert_eq!(format!("{:?}", reason.class()), "Unsupported", "{reason:?}");
    assert!(format!("{reason:?}").contains("GROUPS"), "{reason:?}");
    assert_eq!(
        (header.width(), header.height()),
        (Some(0), Some(1438)),
        "geometry is reported from the axes as written"
    );

    assert!(
        reader.next_image().expect("the walk steps past the groups"),
        "the image extension beyond the group data is reached"
    );
    let header = reader.header().expect("the extension's header");
    assert_eq!(
        (header.width(), header.height()),
        (Some(4), Some(1)),
        "the walk resumed on the extension rather than inside the group data"
    );
}
