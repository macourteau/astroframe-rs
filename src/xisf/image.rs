//! Turning the parsed XML header into one [`Header`] per image occurrence.
//!
//! This is where §11.5's attributes, §11.6's keywords, §11.1's properties and §11.13's
//! references become the format-independent report the rest of the crate hands out.

use crate::error::Result;
use crate::header::PixelStorage;
use crate::header::{DeclineReason, Geometry, Granularity, Header, ImageType, Orientation};
use crate::limits::Limits;
use crate::metadata::{
    Cfa, DisplayChannels, DisplayFunction, KeywordOrigin, KeywordSet, Property, PropertyScope,
    PropertySet, PropertyType, PropertyValue, Resolution, ResolutionUnit,
};
use crate::samples::SampleFormat;
use crate::xisf::attributes::{
    read_bounds, read_byte_order, read_checksum, read_color_space, read_geometry, read_offset,
    read_pixel_storage, read_sample_format,
};
use crate::xisf::block::{
    Checksum, Codec, Compression, Location, Subblock, check_subblock_sums, parse_compression,
    parse_encoding, parse_location, parse_subblocks,
};
use crate::xisf::cache::{Cache, decline_from, malformed, memoized};
use crate::xisf::codec;
use crate::xisf::keywords::{RawKeyword, fold_records, read_keyword};
use crate::xisf::scalars::{parse_float, parse_u32, split_fields};
use crate::xisf::xml::Doc;
use std::sync::Arc;

/// How to get at one image's pixel bytes.
#[derive(Clone, Debug)]
pub(crate) struct BlockPlan {
    /// Where the block lives.
    pub(crate) location: Location,
    /// The `compression` attribute, if any — read from the child `<Data>` element for an
    /// `embedded` block and from the serializing element for every other location mode.
    pub(crate) compression: Option<Compression>,
    /// The `subblocks` list, if any. Shared, not owned: a `BlockPlan` is cloned once per
    /// occurrence of its image, and the list runs to the `Subblock count` cap — 4096 pairs is
    /// 64 KB, which 256 references made 17 MB from a 26 KB header.
    pub(crate) subblocks: Option<Arc<[Subblock]>>,
    /// The `checksum` attribute, if any.
    pub(crate) checksum: Option<Checksum>,
    /// `byteOrder="big"`; absent means little-endian (§10.4).
    pub(crate) big_endian: bool,
    /// `Planar` or `Normal`.
    pub(crate) storage: PixelStorage,
    /// The file's geometry, before any channel narrowing.
    pub(crate) geometry: Geometry,
    /// The stored sample width.
    pub(crate) format: SampleFormat,
    /// Bytes the geometry implies — the cross-check every declared size is measured against,
    /// and the only number a buffer is ever sized from.
    pub(crate) implied_bytes: u64,
    /// For an `embedded` or `inline` block, the bytes decoded during the header parse.
    pub(crate) materialized: Option<Arc<Vec<u8>>>,
}

/// One image occurrence: an `<Image>` element, or a root-level `<Reference>` resolving to one.
///
/// `Clone` is what lets several occurrences of one `<Image>` node share the work of building
/// it; see [`walk_occurrences`]. It stays a *clone* rather than an `Arc` because an occurrence
/// is mutated after the walk — `refuse_attachments_inside_the_header` declines individual
/// positions in place — and because the two heavy fields, the keyword and property lists, are
/// already `Arc`s inside `Header`.
#[derive(Clone, Debug)]
pub(crate) struct Occurrence {
    /// What `Reader::header()` reports at this position.
    pub(crate) header: Header,
    /// How to reach the pixels; `None` at a declined position.
    pub(crate) plan: Option<BlockPlan>,
    /// The file's own `bounds` text, verbatim, whenever the image declared one at all —
    /// usable or not. `with_bounds` reports it so an override never erases the evidence.
    ///
    /// Shared, not owned: `bounds` is an attribute like any other, and one at the
    /// `Attribute value length` cap on an image reached by 256 references retained 276 MB from
    /// a one-megabyte header. `declared_bounds_text` allocates the caller's `String` once, at
    /// the `with_bounds` call that asks for it.
    pub(crate) declared_bounds: Option<Arc<str>>,
}

/// Walk the header's image occurrences in document order.
pub(crate) fn walk_occurrences(doc: &Doc, limits: &Limits) -> Result<Vec<Occurrence>> {
    let mut cache = Cache::default();
    // Root-level `Metadata` properties (§11.4) apply to every image in the unit, so they are
    // read once and reported at their document position in every image's list below.
    let metadata = metadata_properties(doc, &mut cache);
    // Built once and shared by every image, whatever it adds of its own. No sort is needed:
    // `metadata_properties` walks the root's children in document order and node indices ascend
    // with that walk, so the list is already ordered — which holds across several `<Metadata>`
    // elements as well as within one, and "they all come from one element" was the wrong reason
    // to give for it.
    let shared_metadata: Arc<[Property]> = metadata.iter().map(|(_, p)| p.clone()).collect();

    let mut out = Vec::new();
    // One `<Image uid>` reached through N root-level `<Reference>` elements is N occurrences of
    // the *same* node, and § XISF decisions requires each to be reported. Building each from the
    // node rebuilt that image's whole keyword list, its whole merged property list and its
    // `Header` every time: an image with 80 000 `<FITSKeyword>` children reached 256 times — a
    // 3.9 MB header, inside every shipped cap — allocated 4.6 GB and retained 2.1 GB, and the
    // property twin retained 1.5 GB. That is invariant I5's unbounded-allocation clause, on the
    // header-only path a consumer runs over an untrusted size-capped prefix.
    //
    // The built occurrence is memoized on the image's node index instead. That is the whole key
    // because `build_occurrence` reads nothing that varies with the occurrence: the merged
    // property order is keyed on the position of the element each property was *found at* —
    // the image's own child index, or the `Metadata` child's — and the root-level `Reference`
    // that reached the image contributes no position to it. Two references at different
    // positions therefore build byte-identical occurrences, and so does the direct element.
    //
    // **Only reference-reached occurrences are memoized**, for the reason [`Cache`] gives:
    // a direct `<Image>` child appears once by construction, so a map over those is pure
    // overhead — and the element-heavy shape is 40 000 distinct images.
    //
    // This one stays local rather than joining [`Cache`] because the walk itself runs once per
    // document, so its memo is document-scoped already; the child readers are the ones that
    // needed lifting out of their calls.
    let mut built: std::collections::HashMap<usize, Occurrence> = std::collections::HashMap::new();
    for &child in doc.children(doc.root) {
        // §11.13's second worked example rewrites four identical images as one `<Image uid>`
        // plus three bare root-level references, so a walk over `Image` elements alone would
        // report one image on that conforming file. Only *root-level* references count: an
        // image is not a metadata element of another image, and counting a nested one would
        // make the walk depend on nesting.
        let (image, referenced) = match doc.name(child) {
            "Image" => (child, false),
            "Reference" => match doc.resolve(child) {
                Some(target) if doc.name(target) == "Image" => (target, true),
                _ => continue,
            },
            _ => continue,
        };
        out.push(match built.get(&image) {
            Some(shared) => shared.clone(),
            None => {
                let fresh =
                    build_occurrence(doc, limits, image, &metadata, &shared_metadata, &mut cache)?;
                if referenced {
                    built.insert(image, fresh.clone());
                }
                fresh
            }
        });
        // Stop one past the cap. `Images per source` bounds how many occurrences a caller can
        // ever *advance to*, so building more than that is work no caller can reach — and it
        // is not free work: an occurrence carries a whole `Header`, several `Vec`s and four
        // `Arc`s, so a header declaring forty thousand `<Image>` elements allocated a hundred
        // megabytes from four megabytes of input and falsified the fuzz oracle's bound.
        //
        // One *past* the cap rather than at it, so the behaviour a caller sees is unchanged:
        // the cap surfaces at `next_image()` (§ The caps), and the advance that trips it is
        // the one onto the occurrence beyond the last admitted. Truncating at the cap exactly
        // would end the walk cleanly instead, turning a `LimitExceeded` into a short read.
        if out.len() as u64 > u64::from(limits.images_per_source) {
            break;
        }
    }
    // A unit whose walk finds no image occurrence is not an error: construction succeeds and
    // the walk ends normally, exactly as a FITS primary with NAXIS = 0 and no image extension
    // does.
    Ok(out)
}

// ------------------------------------------------------------------ one occurrence

fn build_occurrence(
    doc: &Doc,
    limits: &Limits,
    image: usize,
    metadata: &[(usize, Property)],
    shared_metadata: &Arc<[Property]>,
    cache: &mut Cache,
) -> Result<Occurrence> {
    let (geometry, geometry_fault) = read_geometry(doc.attr(image, "geometry"));
    let (color_space, color_space_fault) = read_color_space(doc.attr(image, "colorSpace"));
    let (sample_format, sample_format_fault) = read_sample_format(doc.attr(image, "sampleFormat"));
    let (big_endian, byte_order_fault) = read_byte_order(doc.attr(image, "byteOrder"));
    let (storage, pixel_storage_fault) = read_pixel_storage(doc.attr(image, "pixelStorage"));

    let site = read_block_site(doc, image);
    let implied_bytes = implied_bytes(geometry, sample_format);
    let stored = stored_size(site.location.as_ref(), site.materialized.as_deref());
    let (compression, subblocks, compression_fault) =
        read_compression(doc, limits, site.attrs_on, stored, implied_bytes);
    // Shared here, once, rather than in the `BlockPlan` literal below: from this point on the
    // list is read through `Arc`, so every occurrence of this image holds the same one.
    let subblocks: Option<Arc<[Subblock]>> = subblocks.map(Arc::from);
    let (offset, offset_fault) = read_offset(doc.attr(image, "offset"));
    let (checksum, checksum_fault) = read_checksum(doc.attr(image, "checksum"));

    // The XISF header-phase validation order, first error wins. It is load-bearing: several
    // adversarial fixtures carry an unsupported *attribute* and no `location` at all, and
    // they yield `Unsupported` only because the location check runs last.
    let decline_reason = geometry_fault
        .or(color_space_fault)
        .or(sample_format_fault)
        .or(byte_order_fault)
        .or(pixel_storage_fault)
        .or(site.fault)
        .or(compression_fault)
        .or(offset_fault)
        // `checksum` is not one of the eight positions the order names, so a fault in it is
        // taken last rather than allowed to preempt one that is.
        .or(checksum_fault);

    let collected = collect_children(doc, image, cache);

    // The caps and `bounds` sit deliberately outside that order: `bounds` is evaluated at
    // parse and raised at normalize, so an invalid one is not a decline.
    let declared_bounds: Option<Arc<str>> = doc.attr(image, "bounds").map(Arc::from);
    let bounds = read_bounds(declared_bounds.as_deref(), sample_format);

    let plan = match (
        &decline_reason,
        geometry,
        sample_format,
        storage,
        &site.location,
    ) {
        (None, Some(geometry), Some(format), Some(storage), Some(location)) => Some(BlockPlan {
            location: location.clone(),
            compression,
            subblocks: subblocks.clone(),
            checksum: checksum.clone(),
            big_endian,
            storage,
            geometry,
            format,
            implied_bytes,
            materialized: site.materialized.clone(),
        }),
        _ => None,
    };

    let granularity = granularity(
        site.location.as_ref(),
        compression.as_ref(),
        subblocks.as_deref(),
        checksum.as_ref(),
        decline_reason.is_some(),
    );

    // §11.4's root `Metadata` properties apply to *every* image, so the merged list is the
    // whole document's properties once per image: 40 000 root properties beside 256 images that
    // each add one of their own — every count inside its cap — allocated 3.2 GB and retained
    // 2.2 GB from a 1.9 MB header. Sharing the built values does not reach it, their texts being
    // `Arc<str>` already; what multiplied was the merge.
    //
    // So the merge is not built. The root list is shared, the image's own properties are its
    // own, and the position the two interleave at is one index, because node indices are
    // assigned in document order and this image's own properties are exactly the ones inside
    // its subtree — the position of a `Property` child, or of the `Reference` standing in for a
    // root-level one, which is what pins the order the specification leaves undefined.
    // `PropertySet` is where that argument is written down and `Header::properties` serves the
    // concatenation as a view.
    let split = metadata.partition_point(|(position, _)| *position < image);
    debug_assert!(
        collected
            .properties
            .iter()
            .all(|(position, _)| *position > image)
            && metadata[split..].first().is_none_or(|(first, _)| {
                collected.properties.iter().all(|(own, _)| own < first)
            }),
        "an image's own properties fall between two root Metadata properties, never among them"
    );
    let own: Arc<[Property]> = collected.properties.into_iter().map(|(_, p)| p).collect();
    let properties = PropertySet::new(shared_metadata.clone(), own, split);

    let header = Header {
        geometry,
        sample_format,
        bounds,
        // `None` unambiguously means XISF: FITS always reports `Fits`, materializing its
        // defaults.
        scaling: None,
        // XISF does not have the `ROWORDER` concept.
        row_order: None,
        orientation: Some(
            doc.attr(image, "orientation")
                .map(Orientation::classify)
                .unwrap_or(Orientation::Identity),
        ),
        offset: Some(offset),
        color_space,
        pixel_storage: storage,
        image_id: doc.attr(image, "id").map(Arc::from),
        image_uuid: doc.attr(image, "uuid").map(Arc::from),
        image_type: doc.attr(image, "imageType").map(ImageType::classify),
        channel_index: None,
        granularity,
        decline_reason,
        // `Arc::default()` for the second piece: an XISF image's keywords are all its own —
        // its `FITSKeyword` children and the `Reference` elements standing in for root-level
        // ones — so there is no inherited list. See `KeywordSet`, which the FITS side needs.
        keywords: KeywordSet::new(
            fold_records(&collected.keywords, cache, limits)?.into(),
            Arc::default(),
        ),
        properties,
        cfa: collected.cfa,
        // XISF does define both, so absence is the specification's default rather than
        // absence of the concept: 72.0 ppi (§11.11) and the identity display function (§11.9).
        resolution: Some(collected.resolution.unwrap_or_default()),
        display_function: Some(collected.display_function.unwrap_or_default()),
    };

    Ok(Occurrence {
        header,
        plan,
        declared_bounds,
    })
}

// ------------------------------------------------------------------ the data block

/// Where an image's pixel block lives, and which element carries the attributes describing how
/// it was compressed.
struct BlockSite {
    location: Option<Location>,
    /// The element `compression` and `subblocks` are read from: the child `<Data>` element for
    /// an `embedded` block and the serializing element for every other location mode (§10.6).
    /// Reading them from the wrong element yields a block that looks uncompressed and decodes
    /// to noise.
    attrs_on: usize,
    /// The bytes of an in-header block, decoded during the header parse.
    materialized: Option<Arc<Vec<u8>>>,
    fault: Option<DeclineReason>,
}

fn read_block_site(doc: &Doc, image: usize) -> BlockSite {
    let mut site = BlockSite {
        location: None,
        attrs_on: image,
        materialized: None,
        fault: None,
    };

    let Some(text) = doc.attr(image, "location") else {
        // §10 requires a block's location and role to be completely defined by the header and
        // §11.5 requires an image's pixels to be a single data block, so this follows even
        // though §11.5.1's attribute list does not name it.
        site.fault = Some(malformed(
            "location: an Image element with no location attribute declares no pixel data",
        ));
        return site;
    };
    let location = match parse_location(text) {
        Ok(location) => location,
        Err(e) => {
            site.fault = Some(decline_from(e));
            return site;
        }
    };

    match &location {
        Location::Inline(_) => {
            // §11.5 is explicit that an Image element cannot serialize pixel data as an inline
            // block, because Image elements may have child elements.
            site.fault = Some(malformed(format!(
                "location {text:?}: §11.5 forbids an Image element from serializing its pixel \
                 data as an inline block; embedded is the in-header spelling"
            )));
        }
        Location::External(_) => {
            site.fault = Some(malformed(format!(
                "location {text:?}: §10.2 forbids an external data block in a monolithic unit"
            )));
        }
        Location::Embedded => {
            match doc.children(image).iter().find(|&&c| doc.name(c) == "Data") {
                None => {
                    site.fault = Some(malformed(
                        "location embedded: §10.3 serializes the block in a child Data element, \
                         and this image has none",
                    ));
                }
                Some(&data) => {
                    site.attrs_on = data;
                    match doc.attr(data, "encoding").map(parse_encoding) {
                        None => {
                            site.fault = Some(malformed(
                                "location embedded: §10.3 requires an encoding attribute on the \
                                 child Data element",
                            ));
                        }
                        Some(Err(e)) => site.fault = Some(decline_from(e)),
                        // The bytes live in the header region, so they are decoded here.
                        Some(Ok(encoding)) => match codec::decode_text(doc.text(data), encoding) {
                            Ok(bytes) => site.materialized = Some(Arc::new(bytes)),
                            Err(e) => site.fault = Some(decline_from(e)),
                        },
                    }
                }
            }
        }
        Location::Attachment { .. } => {}
    }

    site.location = Some(location);
    site
}

/// `width × height × channels × sample_bytes`, in `u64`.
///
/// A geometry whose product overflows saturates rather than declining here: the cap that
/// refuses such a declaration is the total-samples cap, which the validation order puts in the
/// pixel phase deliberately, and declining at the header phase would reclassify it.
fn implied_bytes(geometry: Option<Geometry>, format: Option<SampleFormat>) -> u64 {
    match (geometry, format) {
        (Some(g), Some(f)) => u64::from(g.width)
            .checked_mul(u64::from(g.height))
            .and_then(|n| n.checked_mul(u64::from(g.channels)))
            .and_then(|n| n.checked_mul(u64::from(f.bytes())))
            .unwrap_or(u64::MAX),
        _ => 0,
    }
}

/// The stored length of the block, where the header phase knows it.
fn stored_size(location: Option<&Location>, materialized: Option<&Vec<u8>>) -> Option<u64> {
    match location {
        Some(Location::Attachment { size, .. }) => Some(*size),
        Some(Location::Embedded) => materialized.map(|bytes| bytes.len() as u64),
        _ => None,
    }
}

fn read_compression(
    doc: &Doc,
    limits: &Limits,
    attrs_on: usize,
    stored: Option<u64>,
    implied_bytes: u64,
) -> (
    Option<Compression>,
    Option<Vec<Subblock>>,
    Option<DeclineReason>,
) {
    let compression = match doc.attr(attrs_on, "compression").map(parse_compression) {
        None => None,
        Some(Ok(compression)) => Some(compression),
        Some(Err(e)) => return (None, None, Some(decline_from(e))),
    };
    // §10.6.2's planes are subsets of equally significant bytes of the *uncompressed* block,
    // so an `item-size` larger than that block describes a transform with no complete item in
    // it at all. `parse_compression` cannot see this: the length comes from the geometry, not
    // from the attribute. Skipped when the geometry or the sample format did not read, where
    // the implied size is zero and the validation order has already settled the fault.
    if let Some(item) = compression.and_then(|c| c.item_size)
        && implied_bytes > 0
        && item > implied_bytes
    {
        return (
            compression,
            None,
            Some(malformed(format!(
                "compression item size {item} exceeds the {implied_bytes} bytes the geometry \
                 implies, so §10.6.2's transform has no complete item to define a plane over"
            ))),
        );
    }

    let Some(text) = doc.attr(attrs_on, "subblocks") else {
        return (compression, None, None);
    };
    if compression.is_none() {
        // This crate's decision, not a spec rule: §10.6's "must appear along with the
        // compression attribute" is conditional, and the specification never contemplates
        // `subblocks` on an uncompressed block. The attribute describes how compressed data
        // was split, and on an uncompressed block it describes nothing.
        return (
            None,
            None,
            Some(malformed(format!(
                "subblocks {text:?} appears with no compression attribute, so it describes \
                 nothing"
            ))),
        );
    }
    let subblocks = match parse_subblocks(text, limits) {
        Ok(subblocks) => subblocks,
        Err(e) => return (compression, None, Some(decline_from(e))),
    };
    // Two of the three added checks; the count cap is inside `parse_subblocks`. All three run
    // before any allocation.
    if let Some(stored) = stored
        && let Err(e) = check_subblock_sums(&subblocks, stored, implied_bytes)
    {
        return (compression, Some(subblocks), Some(decline_from(e)));
    }
    (compression, Some(subblocks), None)
}

// ------------------------------------------------------------------ granularity

/// The granularity floors compose, and the granularity is the **worst** of them.
///
/// A first-match implementation gets exactly the interesting combinations wrong: `subblocks`
/// with shuffling, and `subblocks` with a checksum, are both `WholeImage`, because those two
/// floors ignore the subblock split.
fn granularity(
    location: Option<&Location>,
    compression: Option<&Compression>,
    subblocks: Option<&[Subblock]>,
    checksum: Option<&Checksum>,
    declined: bool,
) -> Granularity {
    if declined {
        // No delivery is possible at a declined position and every pixel call errors anyway.
        return Granularity::WholeImage;
    }
    if matches!(location, Some(Location::Embedded)) {
        // The pixels were fully materialized during the header parse, so no part of the input
        // remains to stream.
        return Granularity::WholeImage;
    }

    // The two floors that ignore `subblocks`: the shuffle spans the whole pre-split block, and
    // a digest covers the whole *stored* block (§10.5), which `subblocks` does not split.
    let split_immune = checksum.is_some() || compression.is_some_and(|c| c.shuffled);
    // A bare LZ4 block decompresses only as a whole; zlib and zstd are framed streams that
    // decompress incrementally, so their floor — and an uncompressed block's — is `Rows`.
    let codec_floor_is_block =
        compression.is_some_and(|c| matches!(c.codec, Codec::Lz4 | Codec::Lz4Hc));

    if !split_immune && !codec_floor_is_block {
        return Granularity::Rows;
    }
    // One promotion, applying only to a `Block` floor: it becomes `WholeImage` unless the block
    // is split into subblocks. A `Rows` floor is never promoted, which is why `subblocks` never
    // lowers zlib's.
    match subblocks {
        Some(list) if !split_immune => Granularity::Block {
            subblocks: u32::try_from(list.len()).unwrap_or(u32::MAX),
        },
        _ => Granularity::WholeImage,
    }
}

// ------------------------------------------------------------------ the child elements

/// What one image's children contribute, in document order.
#[derive(Default)]
struct Collected<'a> {
    keywords: Vec<RawKeyword<'a>>,
    properties: Vec<(usize, Property)>,
    cfa: Option<Cfa>,
    resolution: Option<Resolution>,
    display_function: Option<DisplayFunction>,
}

/// Walk one image's children, resolving each `Reference` exactly one hop.
///
/// A referenced element takes the position of its `Reference`, which is what pins the order the
/// specification leaves undefined. The core elements this crate meets and does not read are
/// dispositioned explicitly rather than left to fall through the ignore-unknown rule.
fn collect_children<'a>(doc: &'a Doc, image: usize, cache: &mut Cache) -> Collected<'a> {
    let mut out = Collected::default();
    // One root-level `<Property>`, `<ColorFilterArray>`, `<Resolution>` or `<DisplayFunction>`
    // reached through N in-image `<Reference>` elements is N reads of the same element, and
    // reading it per occurrence copied its texts every time: 4096 references to one 48 KB
    // property — a 130 KB header, inside every cap — allocated 200 MB and retained 199 MB.
    //
    // The memo is [`Cache`]'s, not this call's: a memo built here would be rebuilt per image,
    // so 256 *distinct* images each referencing one root element paid the read 256 times over
    // — 275 MB from a one-megabyte header for a root `<ColorFilterArray>` at the
    // `Attribute value length` cap.
    for &child in doc.children(image) {
        let (node, origin) = if doc.name(child) == "Reference" {
            match doc.resolve(child) {
                Some(target) => (target, KeywordOrigin::Reference),
                None => continue,
            }
        } else {
            (child, KeywordOrigin::Image)
        };

        let referenced = origin == KeywordOrigin::Reference;
        match doc.name(node) {
            "FITSKeyword" => {
                if let Some(keyword) = read_keyword(doc, node, origin) {
                    out.keywords.push(keyword);
                }
            }
            "Property" => {
                // Tagged with the scope of the element it attaches *to*, not the root — which
                // is `Image` on both paths here, and the scope is part of the memo key, so a
                // hit needs no adjustment.
                let property = memoized(
                    &mut cache.properties,
                    (node, PropertyScope::Image),
                    referenced,
                    || read_property(doc, node, PropertyScope::Image),
                );
                if let Some(property) = property {
                    out.properties.push((child, property));
                }
            }
            "ColorFilterArray" => {
                if out.cfa.is_none() {
                    out.cfa = memoized(&mut cache.cfa, node, referenced, || read_cfa(doc, node));
                }
            }
            "Resolution" => {
                if out.resolution.is_none() {
                    out.resolution =
                        Some(memoized(&mut cache.resolution, node, referenced, || {
                            read_resolution(doc, node)
                        }));
                }
            }
            "DisplayFunction" => {
                if out.display_function.is_none() {
                    // Memoized for a transient rather than a retained copy: a
                    // `DisplayFunction` is five `f64` quadruples and holds no text, but
                    // reading one splits five attributes into field vectors, and an attribute
                    // at the 1 MiB cap is an eight-megabyte vector per read.
                    out.display_function = Some(memoized(
                        &mut cache.display_function,
                        node,
                        referenced,
                        || read_display_function(doc, node),
                    ));
                }
            }
            // Declined silently: an element never fails a frame it does not prevent decoding.
            // `RGBWorkingSpace` appears in §11.13's own worked example and PixInsight writes it
            // routinely, so treating it as frame-level would refuse a large share of real RGB
            // files. `Data` is the embedded block, read at the location step; an `Image`
            // reached from inside an image is not an occurrence and contributes nothing.
            "ICCProfile" | "RGBWorkingSpace" | "Table" | "Structure" | "Thumbnail" | "Data"
            | "Image" => {}
            // Unknown elements are ignored: the specification states no forward-compatibility
            // rule anywhere, and ignoring unknowns is the only reading under which a 1.0
            // decoder survives a later revision.
            _ => {}
        }
    }
    out
}

/// An XISF `ColorFilterArray` (§11.10). It is the one mosaic-and-display accessor with no
/// default: absence means the image is not mosaiced.
fn read_cfa(doc: &Doc, node: usize) -> Option<Cfa> {
    Some(Cfa {
        pattern: Arc::from(doc.attr(node, "pattern")?),
        width: doc.attr(node, "width").and_then(parse_u32)?,
        height: doc.attr(node, "height").and_then(parse_u32)?,
        name: doc.attr(node, "name").map(Arc::from),
    })
}

/// An XISF `Resolution` (§11.11), whose 72.0 ppi default [`Resolution::default`] carries.
fn read_resolution(doc: &Doc, node: usize) -> Resolution {
    let axis = |name: &str| {
        doc.attr(node, name)
            .and_then(parse_float)
            .filter(|v| *v > 0.0)
            .unwrap_or(72.0)
    };
    Resolution {
        horizontal: axis("horizontal"),
        vertical: axis("vertical"),
        unit: match doc.attr(node, "unit") {
            Some("cm") => ResolutionUnit::Centimetre,
            _ => ResolutionUnit::Inch,
        },
    }
}

/// An XISF `DisplayFunction` (§11.9), reported and applied to nothing.
fn read_display_function(doc: &Doc, node: usize) -> DisplayFunction {
    let identity = DisplayFunction::default();
    let channels = |name: &str, fallback: DisplayChannels| -> DisplayChannels {
        let Some(text) = doc.attr(node, name) else {
            return fallback;
        };
        let fields = split_fields(text);
        let [red_gray, green, blue, lightness] = fields[..] else {
            return fallback;
        };
        match (
            parse_float(red_gray),
            parse_float(green),
            parse_float(blue),
            parse_float(lightness),
        ) {
            (Some(red_gray), Some(green), Some(blue), Some(lightness)) => DisplayChannels {
                red_gray,
                green,
                blue,
                lightness,
            },
            _ => fallback,
        }
    };
    DisplayFunction {
        midtones: channels("m", identity.midtones()),
        shadows: channels("s", identity.shadows()),
        highlights: channels("h", identity.highlights()),
        low_range: channels("l", identity.low_range()),
        high_range: channels("r", identity.high_range()),
    }
}

/// One XISF `Property` (§11.1), reported as a tuple and never parsed per its declared type.
fn read_property(doc: &Doc, node: usize, scope: PropertyScope) -> Option<Property> {
    let value = if let Some(text) = doc.attr(node, "value") {
        PropertyValue::Text(Arc::from(text))
    } else if doc.attr(node, "location").is_some() {
        // Reported rather than silently dropped, so a consumer can tell "the file does not
        // carry this property" from "it does and this version cannot read it". The external
        // `url(…)`/`path(…)` forms land here too rather than raising the `Malformed` that the
        // same spelling raises on an image's *pixel* block: a property never prevents decoding.
        PropertyValue::Unavailable
    } else {
        // §11.1.6: a `String` property *shall not* carry a `value` attribute and serializes its
        // value as character data, all of whose white space is significant.
        PropertyValue::Text(Arc::from(doc.text(node)))
    };
    Some(Property {
        // Verbatim and never validated as a token: a space-bearing id such as
        // `"Instrument: colorFlag"` has been reported in the wild.
        id: Arc::from(doc.attr(node, "id")?),
        property_type: PropertyType::classify(doc.attr(node, "type").unwrap_or("")),
        value,
        format: doc.attr(node, "format").map(Arc::from),
        comment: doc.attr(node, "comment").map(Arc::from),
        scope,
    })
}

/// Root-level `Metadata`-scope properties (§11.4), which apply to every image in the unit.
///
/// A `FITSKeyword` here is a non-conforming placement (§11.6) and is ignored rather than
/// reported: it is attached to no image, and reporting it against an arbitrary one would invent
/// an association the file does not make.
fn metadata_properties(doc: &Doc, cache: &mut Cache) -> Vec<(usize, Property)> {
    let mut out = Vec::new();
    // Memoized exactly as `collect_children` is, through the same [`Cache`], and for the same
    // shape one level up: several `<Reference>` elements inside `<Metadata>` may resolve to one
    // root-level `<Property>`. The `Metadata` scope is part of the key, so an image-scope read
    // of the same node neither hits this nor is hit by it.
    for &child in doc.children(doc.root) {
        if doc.name(child) != "Metadata" {
            continue;
        }
        for &node in doc.children(child) {
            let (target, referenced) = if doc.name(node) == "Reference" {
                match doc.resolve(node) {
                    Some(target) => (target, true),
                    None => continue,
                }
            } else {
                (node, false)
            };
            if doc.name(target) != "Property" {
                continue;
            }
            let property = memoized(
                &mut cache.properties,
                (target, PropertyScope::Metadata),
                referenced,
                || read_property(doc, target, PropertyScope::Metadata),
            );
            if let Some(property) = property {
                out.push((node, property));
            }
        }
    }
    out
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::header::{Bounds, DeclineClass};

    fn walk(header: &str) -> Vec<Occurrence> {
        let limits = Limits::default();
        let doc = crate::xisf::xml::parse(header.as_bytes(), &limits).expect("header parses");
        walk_occurrences(&doc, &limits).expect("the walk succeeds")
    }

    pub(crate) fn one(header: &str) -> Occurrence {
        let mut occurrences = walk(header);
        assert_eq!(occurrences.len(), 1, "expected one image occurrence");
        occurrences.remove(0)
    }

    /// A one-image unit carrying exactly the attributes the caller names.
    pub(crate) fn image(image_attrs: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
               <xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">
                 <Image {image_attrs}/>
               </xisf>"#
        )
    }

    /// A 4×4 `UInt8` image is 16 bytes, which keeps the subblock sum arithmetic small.
    pub(crate) fn tiny(image_attrs: &str) -> String {
        image(&format!(
            r#"geometry="4:4:1" sampleFormat="UInt8" {image_attrs}"#
        ))
    }

    pub(crate) fn class(occurrence: &Occurrence) -> Option<DeclineClass> {
        occurrence.header.decline_reason().map(|d| d.class())
    }

    // -------------------------------------------------------------- granularity floors

    #[test]
    fn an_uncompressed_unchecksummed_attachment_streams_by_rows() {
        let o = one(&tiny(r#"location="attachment:1024:16""#));
        assert_eq!(o.header.granularity(), Granularity::Rows);
        assert!(o.plan.is_some());
    }

    #[test]
    fn zlib_streams_by_rows_and_subblocks_cannot_lower_that_floor() {
        let plain = one(&tiny(
            r#"location="attachment:1024:10" compression="zlib:16""#,
        ));
        assert_eq!(plain.header.granularity(), Granularity::Rows);

        // `subblocks` only blocks a promotion; it never lowers a `Rows` floor.
        let split = one(&tiny(
            r#"location="attachment:1024:10" compression="zlib:16" subblocks="5,8:5,8""#,
        ));
        assert_eq!(split.header.granularity(), Granularity::Rows);
    }

    #[test]
    fn lz4_reaches_block_granularity_only_through_subblocks() {
        let split = one(&tiny(
            r#"location="attachment:1024:10" compression="lz4:16" subblocks="5,8:5,8""#,
        ));
        assert_eq!(
            split.header.granularity(),
            Granularity::Block { subblocks: 2 }
        );

        // One bare LZ4 block covering the image is promoted to `WholeImage`.
        let whole = one(&tiny(
            r#"location="attachment:1024:10" compression="lz4:16""#,
        ));
        assert_eq!(whole.header.granularity(), Granularity::WholeImage);
    }

    #[test]
    fn shuffling_and_checksums_ignore_the_subblock_split() {
        // The shuffle spans the whole pre-split block, so subblock boundaries buy nothing.
        let shuffled = one(&tiny(
            r#"location="attachment:1024:10" compression="lz4+sh:16:2" subblocks="5,8:5,8""#,
        ));
        assert_eq!(shuffled.header.granularity(), Granularity::WholeImage);

        // The digest covers the whole stored block, which `subblocks` does not split.
        let checksummed = one(&tiny(
            r#"location="attachment:1024:10" compression="lz4:16" subblocks="5,8:5,8"
               checksum="sha1:97b25345e3bd74bcd6613d24e3ecb47617a31d20""#,
        ));
        assert_eq!(checksummed.header.granularity(), Granularity::WholeImage);

        // zlib's floor is `Rows`, but a shuffle raises it and the promotion takes it home.
        let zlib_shuffled = one(&tiny(
            r#"location="attachment:1024:10" compression="zlib+sh:16:2" subblocks="5,8:5,8""#,
        ));
        assert_eq!(zlib_shuffled.header.granularity(), Granularity::WholeImage);

        // The worst of the floors, not the first found.
        let all = one(&tiny(
            r#"location="attachment:1024:10" compression="zlib+sh:16:2" subblocks="5,8:5,8"
               checksum="sha1:97b25345e3bd74bcd6613d24e3ecb47617a31d20""#,
        ));
        assert_eq!(all.header.granularity(), Granularity::WholeImage);
    }

    #[test]
    fn an_embedded_block_is_whole_image_and_a_declined_position_is_too() {
        let embedded = one(
            r#"<xisf version="1.0"><Image geometry="2:2:1" sampleFormat="UInt8"
                 location="embedded"><Data encoding="hex">00010203</Data></Image></xisf>"#,
        );
        assert_eq!(embedded.header.granularity(), Granularity::WholeImage);

        let declined = one(&image(
            r#"geometry="4:4:1" sampleFormat="Nope" location="attachment:1024:16""#,
        ));
        assert_eq!(declined.header.granularity(), Granularity::WholeImage);
    }

    #[test]
    fn subblocks_without_compression_is_malformed() {
        let o = one(&tiny(
            r#"location="attachment:1024:16" subblocks="8,8:8,8""#,
        ));
        assert_eq!(class(&o), Some(DeclineClass::Malformed));
    }

    #[test]
    fn the_subblock_sums_are_checked_against_the_stored_and_implied_sizes() {
        let o = one(&tiny(
            r#"location="attachment:1024:10" compression="lz4:16" subblocks="5,8:5,9""#,
        ));
        assert_eq!(class(&o), Some(DeclineClass::Malformed));
    }

    #[test]
    fn a_subblock_count_above_the_cap_declines_the_position_rather_than_the_unit() {
        let limits = Limits {
            subblock_count: 1,
            ..Default::default()
        };
        let header =
            tiny(r#"location="attachment:1024:10" compression="lz4:16" subblocks="5,8:5,8""#);
        let doc = crate::xisf::xml::parse(header.as_bytes(), &limits).expect("header parses");
        let occurrences = walk_occurrences(&doc, &limits).expect("the walk succeeds");
        assert_eq!(
            occurrences[0].header.decline_reason().map(|d| d.class()),
            Some(DeclineClass::LimitExceeded)
        );
    }

    // -------------------------------------------------------------- occurrences

    #[test]
    fn a_unit_with_no_image_walks_zero_occurrences() {
        let occurrences = walk(
            r#"<xisf version="1.0"><Metadata>
                 <Property id="XISF:CreationTime" type="TimePoint" value="2026-01-01"/>
               </Metadata></xisf>"#,
        );
        assert!(occurrences.is_empty());
    }

    #[test]
    fn the_deduplicated_spelling_reports_one_occurrence_per_reference() {
        // §11.13's second worked example, in miniature.
        let occurrences = walk(
            r#"<xisf version="1.0">
                 <Image uid="foo_bar" geometry="4:4:1" sampleFormat="UInt8"
                        location="attachment:1024:16"/>
                 <Reference ref="foo_bar"/>
                 <Reference ref="foo_bar"/>
                 <Reference ref="foo_bar"/>
               </xisf>"#,
        );
        assert_eq!(occurrences.len(), 4);
        for occurrence in &occurrences {
            assert_eq!(occurrence.header.width(), Some(4));
            assert_eq!(class(occurrence), None);
        }
    }

    #[test]
    fn a_forward_reference_to_an_image_resolves_and_a_dangling_one_does_not() {
        let forward = walk(
            r#"<xisf version="1.0">
                 <Reference ref="later"/>
                 <Image uid="later" geometry="4:4:1" sampleFormat="UInt8"
                        location="attachment:1024:16"/>
               </xisf>"#,
        );
        assert_eq!(forward.len(), 2, "a forward reference is an occurrence");

        let dangling = walk(r#"<xisf version="1.0"><Reference ref="nobody"/></xisf>"#);
        assert!(dangling.is_empty());
    }

    #[test]
    fn a_reference_to_an_image_inside_an_image_is_not_an_occurrence() {
        let occurrences = walk(
            r#"<xisf version="1.0">
                 <Image uid="inner" geometry="4:4:1" sampleFormat="UInt8"
                        location="attachment:1024:16"/>
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:2048:16">
                   <Reference ref="inner"/>
                 </Image>
               </xisf>"#,
        );
        assert_eq!(occurrences.len(), 2);
        // It contributes nothing to the enclosing image either.
        assert!(occurrences[1].header.keywords().is_empty());
        assert!(occurrences[1].header.properties().is_empty());
    }

    // -------------------------------------------------------------- properties

    #[test]
    fn the_three_scopes_merge_in_document_order() {
        let o = one(r#"<xisf version="1.0">
                 <Metadata>
                   <Property id="XISF:CreationTime" type="TimePoint" value="2026-01-01"/>
                 </Metadata>
                 <Property uid="shared" id="Observation:Time:Start" type="TimePoint"
                           value="2026-02-02"/>
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <Reference ref="shared"/>
                   <Property id="Instrument:ExposureTime" type="Float32" value="300"/>
                 </Image>
               </xisf>"#);
        let ids: Vec<&str> = o.header.properties().iter().map(|p| p.id()).collect();
        assert_eq!(
            ids,
            vec![
                "XISF:CreationTime",
                "Observation:Time:Start",
                "Instrument:ExposureTime"
            ]
        );
        let scopes: Vec<PropertyScope> = o.header.properties().iter().map(|p| p.scope()).collect();
        assert_eq!(
            scopes,
            vec![
                PropertyScope::Metadata,
                // Tagged with the scope of the element it attaches to, not the root.
                PropertyScope::Image,
                PropertyScope::Image
            ]
        );
    }

    #[test]
    fn another_images_child_properties_are_not_included() {
        let occurrences = walk(
            r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <Property id="Image:One" type="String">first</Property>
                 </Image>
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:2048:16">
                   <Property id="Image:Two" type="String">second</Property>
                 </Image>
                 <Property id="Unreferenced" type="String">nobody</Property>
               </xisf>"#,
        );
        assert_eq!(occurrences.len(), 2);
        let first: Vec<&str> = occurrences[0]
            .header
            .properties()
            .iter()
            .map(|p| p.id())
            .collect();
        assert_eq!(first, vec!["Image:One"]);
        let second: Vec<&str> = occurrences[1]
            .header
            .properties()
            .iter()
            .map(|p| p.id())
            .collect();
        assert_eq!(
            second,
            vec!["Image:Two"],
            "an unreferenced root-level property is attached to no image"
        );
    }

    #[test]
    fn a_string_propertys_white_space_is_preserved_exactly() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <Property id="Observation:Object:Name" type="String">  M 31  </Property>
                 </Image>
               </xisf>"#);
        let property = &o.header.properties()[0];
        assert_eq!(property.property_type(), &PropertyType::String);
        assert_eq!(property.value(), &PropertyValue::Text("  M 31  ".into()));
    }

    #[test]
    fn a_block_valued_property_is_reported_unavailable_rather_than_dropped() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <Property id="Processing:History" type="String"
                             location="attachment:4096:8192" comment="what happened"/>
                   <Property id="AstrometricSolution" type="F64Matrix"
                             location="path(@header_dir/astrometry/solution.dat)"/>
                 </Image>
               </xisf>"#);
        let properties = o.header.properties();
        assert_eq!(properties.len(), 2);
        assert_eq!(properties[0].value(), &PropertyValue::Unavailable);
        assert_eq!(properties[0].comment(), Some("what happened"));
        // The external form is reported this way too rather than raising a `Malformed`.
        assert_eq!(properties[1].value(), &PropertyValue::Unavailable);
        assert_eq!(properties[1].property_type(), &PropertyType::F64Matrix);
        assert_eq!(class(&o), None, "a property never prevents decoding");
    }

    #[test]
    fn a_property_id_is_reported_verbatim_and_never_validated() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <Property id="Instrument: colorFlag" type="Boolean" value="1"/>
                 </Image>
               </xisf>"#);
        assert_eq!(o.header.properties()[0].id(), "Instrument: colorFlag");
    }

    // -------------------------------------------------------------- keywords

    #[test]
    fn a_root_level_keyword_reaches_an_image_only_through_a_reference() {
        let occurrences = walk(
            r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <FITSKeyword name="OWN" value="1" comment=""/>
                   <Reference ref="KWD001"/>
                 </Image>
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:2048:16"/>
                 <FITSKeyword uid="KWD001" name="SHARED" value="2" comment="shared"/>
                 <FITSKeyword name="ORPHAN" value="3" comment="nobody references this"/>
               </xisf>"#,
        );
        let first = occurrences[0].header.keywords();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].name(), "OWN");
        assert_eq!(first[0].origin(), KeywordOrigin::Image);
        // The referenced keyword takes the position of the `Reference`.
        assert_eq!(first[1].name(), "SHARED");
        assert_eq!(first[1].origin(), KeywordOrigin::Reference);
        // A root-level keyword nothing references appears nowhere.
        assert!(occurrences[1].header.keywords().is_empty());
    }

    #[test]
    fn a_fits_keyword_inside_metadata_is_ignored_rather_than_reported() {
        let o = one(r#"<xisf version="1.0">
                 <Metadata>
                   <FITSKeyword name="STRAY" value="1" comment="wrong parent"/>
                 </Metadata>
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16"/>
               </xisf>"#);
        assert!(o.header.keywords().is_empty());
        assert!(o.header.get("STRAY").is_none());
    }

    // -------------------------------------------------------------- the other elements

    #[test]
    fn the_mosaic_and_display_elements_are_reported_through_reference_too() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <ColorFilterArray pattern="GRBG" width="2" height="2" name="GRBG Bayer"/>
                   <Reference ref="PrintingResolution"/>
                   <DisplayFunction m="0.000735:0.000735:0.000735:0.5"
                                    s="0.003758:0.003758:0.003758:0"
                                    h="1:1:1:1" l="0:0:0:0" r="1:1:1:1" name="AutoStretch"/>
                 </Image>
                 <Resolution uid="PrintingResolution" horizontal="120" vertical="120" unit="cm"/>
               </xisf>"#);
        let cfa = o.header.cfa().expect("the CFA is reported");
        assert_eq!(cfa.pattern(), "GRBG");
        assert_eq!((cfa.width(), cfa.height()), (2, 2));
        assert_eq!(cfa.name(), Some("GRBG Bayer"));

        let resolution = o.header.resolution().expect("XISF states a resolution");
        assert_eq!(resolution.horizontal(), 120.0);
        assert_eq!(resolution.unit(), ResolutionUnit::Centimetre);

        let df = o
            .header
            .display_function()
            .expect("XISF states a display function");
        assert_eq!(df.midtones().red_gray, 0.000735);
        assert_eq!(df.midtones().lightness, 0.5);
        assert_eq!(df.highlights().blue, 1.0);
    }

    #[test]
    fn the_elements_this_crate_does_not_read_are_declined_silently() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="attachment:1024:16">
                   <RGBWorkingSpace x="0.64:0.23:0.15" y="0.33:0.70:0.06"
                                    Y="0.31:0.62:0.06" gamma="2.2" name="Adobe RGB (1998)"/>
                   <ICCProfile location="attachment:8192:560"/>
                   <Structure><Field id="a" type="UInt8"/></Structure>
                   <Table rows="2" columns="2"/>
                   <Thumbnail geometry="2:2:1" sampleFormat="UInt8" location="embedded"/>
                   <SomethingFromVersion2 whatever="yes"/>
                 </Image>
               </xisf>"#);
        assert_eq!(class(&o), None, "the frame still decodes");
        assert!(o.header.keywords().is_empty());
        assert!(o.header.properties().is_empty());
    }

    // -------------------------------------------------------------- block plumbing

    #[test]
    fn an_embedded_blocks_compression_is_read_from_its_child_data_element() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="4:4:1" sampleFormat="UInt8" location="embedded">
                   <Data encoding="base64" compression="zlib:16">AAAAAAAAAAAA</Data>
                 </Image>
               </xisf>"#);
        assert_eq!(class(&o), None);
        let plan = o.plan.expect("a decodable position carries a plan");
        let compression = plan
            .compression
            .expect("compression lives on the child Data element for an embedded block");
        assert_eq!(compression.codec, Codec::Zlib);
        assert_eq!(compression.uncompressed_size, 16);
        assert_eq!(plan.implied_bytes, 16);
        assert!(
            plan.materialized.is_some(),
            "in-header bytes are decoded here"
        );
    }

    #[test]
    fn an_embedded_blocks_text_is_decoded_under_the_section_10_3_white_space_rule() {
        let o = one(r#"<xisf version="1.0">
                 <Image geometry="2:2:1" sampleFormat="UInt8" location="embedded">
                   <Data encoding="hex">
                     00010203
                   </Data>
                 </Image>
               </xisf>"#);
        let plan = o.plan.expect("a decodable position carries a plan");
        assert_eq!(
            plan.materialized.as_deref().map(Vec::as_slice),
            Some(&[0u8, 1, 2, 3][..])
        );
    }

    #[test]
    fn an_embedded_block_with_no_data_element_or_no_encoding_is_malformed() {
        let no_data = one(&tiny(r#"location="embedded""#));
        assert_eq!(class(&no_data), Some(DeclineClass::Malformed));

        let no_encoding = one(
            r#"<xisf version="1.0"><Image geometry="2:2:1" sampleFormat="UInt8"
                 location="embedded"><Data>00010203</Data></Image></xisf>"#,
        );
        assert_eq!(class(&no_encoding), Some(DeclineClass::Malformed));
    }

    #[test]
    fn an_inline_or_external_pixel_location_is_malformed() {
        // §11.5: an Image element cannot serialize pixel data as an inline block.
        let inline = one(&tiny(r#"location="inline:base64""#));
        assert_eq!(class(&inline), Some(DeclineClass::Malformed));

        // §10.2 forbids external blocks in a monolithic unit outright.
        let external = one(&tiny(r#"location="url(http://example.invalid/b)""#));
        assert_eq!(class(&external), Some(DeclineClass::Malformed));
    }

    #[test]
    fn the_plan_carries_the_byte_order_and_the_geometry_the_file_declared() {
        let o = one(
            r#"<xisf version="1.0"><Image geometry="4:4:3" sampleFormat="UInt16"
                 pixelStorage="Normal" byteOrder="big"
                 location="attachment:1024:96"/></xisf>"#,
        );
        let plan = o.plan.expect("a decodable position carries a plan");
        assert!(plan.big_endian);
        assert_eq!(plan.storage, PixelStorage::Normal);
        assert_eq!(plan.format, SampleFormat::U16);
        assert_eq!(plan.implied_bytes, 4 * 4 * 3 * 2);
    }

    // -------------------------------------------------------------- parsing surface

    #[test]
    fn a_namespace_prefixed_header_is_matched_by_local_name() {
        let o = one(
            r#"<xisf:xisf version="1.0" xmlns:xisf="http://www.pixinsight.com/xisf">
                 <xisf:Image geometry="4:4:1" sampleFormat="UInt8"
                             location="attachment:1024:16" id="IMG7953">
                   <xisf:FITSKeyword name="EXPTIME" value="300" comment="seconds"/>
                 </xisf:Image>
               </xisf:xisf>"#,
        );
        assert_eq!(class(&o), None);
        assert_eq!(o.header.image_id(), Some("IMG7953"));
        assert_eq!(o.header.keywords().len(), 1);
    }

    #[test]
    fn identity_attributes_are_reported_and_an_unknown_closed_value_degrades_to_text() {
        let o = one(&tiny(
            r#"location="attachment:1024:16" id="IMG7953"
               uuid="c5c93b6d-9072-4e85-9548-1a5391377683" imageType="MasterLight""#,
        ));
        assert_eq!(o.header.image_id(), Some("IMG7953"));
        assert_eq!(
            o.header.image_uuid(),
            Some("c5c93b6d-9072-4e85-9548-1a5391377683")
        );
        assert_eq!(o.header.image_type(), Some(&ImageType::MasterLight));

        // `imageType` and `orientation` are closed enumerations too, but decoding does not
        // depend on either, so an unknown value degrades to "unknown" and is reported as text.
        let unknown = one(&tiny(
            r#"location="attachment:1024:16" imageType="Whatever" orientation="17""#,
        ));
        assert_eq!(class(&unknown), None);
        assert_eq!(
            unknown.header.image_type(),
            Some(&ImageType::Other("Whatever".into()))
        );
        assert_eq!(
            unknown.header.orientation(),
            Some(&Orientation::Other("17".into()))
        );
    }

    #[test]
    fn section_8_3_white_space_is_ignored_inside_the_scalar_attributes() {
        let o = one(&image(
            r#"geometry=" 4 : 4 : 1 " sampleFormat="UInt8"
               location=" attachment: 1024 : 16 " bounds=" 0 : 255 ""#,
        ));
        assert_eq!(class(&o), None);
        assert_eq!(o.header.width(), Some(4));
        assert_eq!(*o.header.bounds(), Bounds::Declared(0.0, 255.0));
    }
}
