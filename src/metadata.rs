//! The two text surfaces — FITS keywords and XISF properties — and the ancillary elements
//! that are reported and applied to nothing.

/// Where a keyword came from.
///
/// Without this the `INHERIT` rule's promise that a caller can tell an extension's own card
/// from the primary's is unimplementable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeywordOrigin {
    /// A card the image itself carries — a FITS extension header's own card, or a
    /// `FITSKeyword` child of the selected `<Image>`.
    Image,
    /// A card inherited from the FITS primary header. Always reported; whether it is
    /// *applied* is what `INHERIT` gates, and only for the four cards that change what a
    /// pixel means.
    PrimaryHeader,
    /// A root-level XISF `FITSKeyword` reaching this image through a `Reference`.
    Reference,
}

/// One keyword, as the file wrote it.
///
/// Values are the text in the file, with FITS quoting removed and trailing blanks stripped
/// per the standard's character-string value rules — and **not** reformatted. Re-rendering a
/// number through a formatter can lose digits, and the consumer is the one that parses.
///
/// This is why FITS permits `D` as the exponent character for double-precision values
/// (`1.234D+02`), which neither Go's nor Rust's float parser accepts: the `D` form is
/// reported as written. Translating it is the consumer's parse step.
///
/// The three texts live in **one** allocation, sliced by two offsets.
///
/// Three `String`s would be three heap allocations and three 24-byte headers per keyword, and
/// a keyword's texts are short enough that the allocator's minimum block dominates them. A
/// header of 49 000 `FITSKeyword` elements — legal, and inside every cap — made that the
/// second-largest buffer in the crate and put header parsing above the allocation bound
/// § Fuzzing sets. Packing them costs one `Box<str>` and two `u32`s instead.
///
/// The accessors return `&str` either way, so this is invisible from outside: `name()`,
/// `value()` and `comment()` keep their signatures, and the type keeps its derives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyword {
    /// `name`, then `value`, then `comment`, concatenated.
    ///
    /// `Arc` rather than `Box`: one `<FITSKeyword>` reached through N `<Reference>` elements
    /// is N occurrences of the same text, and copying it per occurrence let a one-megabyte
    /// header retain a gigabyte. Sharing makes a repeat cost a refcount.
    pub(crate) buf: std::sync::Arc<str>,
    /// Where `name` ends and `value` begins.
    ///
    /// `usize`, not `u32`. A `u32` kept the struct two words smaller and bought a panic: a
    /// caller may raise `xml_header_bytes` or `fits_header_bytes` past 4 GiB, and an offset
    /// that does not fit then either wraps to the wrong substring or saturates to an index
    /// that splits a `char` boundary. Sixteen bytes a keyword is the wrong thing to trade for
    /// a reachable panic, and § The caps requires every narrowing of a file-declared length to
    /// be checked — the cheapest way to satisfy that here is not to narrow at all.
    pub(crate) name_end: usize,
    /// Where `value` ends. `comment` runs from here to the end of `buf` — and `None` is
    /// distinguished from an empty comment by this being `None`, not by the run being empty:
    /// a FITS card carrying no comment and one carrying an empty one are different cards.
    pub(crate) value_end: Option<usize>,
    pub(crate) origin: KeywordOrigin,
}

impl Keyword {
    /// Pack the three texts into one allocation.
    // Both callers are format decoders, so with neither format compiled in this is dead —
    // and § Operations makes the empty feature set a supported build.
    #[cfg(any(feature = "fits", feature = "xisf"))]
    pub(crate) fn new(
        name: &str,
        value: &str,
        comment: Option<&str>,
        origin: KeywordOrigin,
    ) -> Keyword {
        let mut buf = String::with_capacity(name.len() + value.len() + comment.map_or(0, str::len));
        buf.push_str(name);
        buf.push_str(value);
        let name_end = name.len();
        let value_end = comment.map(|c| {
            let at = buf.len();
            buf.push_str(c);
            at
        });
        Keyword {
            buf: std::sync::Arc::from(buf),
            name_end,
            value_end,
            origin,
        }
    }

    /// The keyword name, trimmed of the space-filling FITS mandates. A `HIERARCH` card
    /// carries its full multi-word name here (`ESO DET EXP`), never the bare `HIERARCH`.
    pub fn name(&self) -> &str {
        &self.buf[..self.name_end]
    }

    /// The value text, verbatim. `HISTORY` and `COMMENT` carry an empty value by
    /// specification and their text lives in [`Keyword::comment`].
    pub fn value(&self) -> &str {
        let end = self.value_end.unwrap_or(self.buf.len());
        &self.buf[self.name_end..end]
    }

    /// The comment text. Always present for an XISF-sourced keyword, `comment` being
    /// mandatory in §11.6.1; absent only for a FITS card carrying none.
    pub fn comment(&self) -> Option<&str> {
        self.value_end.map(|e| &self.buf[e..])
    }

    /// Where this card came from.
    pub fn origin(&self) -> KeywordOrigin {
        self.origin
    }
}

/// The keywords reported against one image, held as the two pieces the report concatenates
/// rather than as one merged list.
///
/// § FITS decisions requires both headers' cards to be reported at an image extension — the
/// extension's own followed by the primary's — so the primary's list is a root list that
/// applies to **every** image in the file, which is the same arithmetic root `<Metadata>` has
/// on the XISF side and the same answer [`PropertySet`] gives to it. `FITS header cards` is
/// 4096 and `Images per source` is 256, and nothing bounds their product: a primary carrying
/// 4090 `HISTORY` cards followed by 256 zero-width `IMAGE` extensions — one 2880-byte block
/// each, every count inside its cap — allocated 53 MB and held 52 MB live from a 1.07 MB
/// input. Sharing the built `Keyword` values does not touch it, their texts being `Arc<str>`
/// already; what multiplies is the concatenation.
///
/// It splits far more simply than the property merge does: the two pieces never interleave, so
/// there is no split index, only `own` then `inherited`. That order is the reported one and it
/// is load-bearing — [`crate::Header::get`] returns the first match in stored order, which is
/// what makes an extension's own `EXPTIME` win over the primary's.
///
/// An XISF image and a FITS primary both leave `inherited` empty: an XISF image's keywords are
/// its own children and the `Reference` elements standing in for root-level ones, all of them
/// at positions inside that image, and a primary header inherits from nothing.
#[derive(Clone, Default)]
pub(crate) struct KeywordSet {
    own: std::sync::Arc<[Keyword]>,
    inherited: std::sync::Arc<[Keyword]>,
}

impl KeywordSet {
    // Both callers are format decoders; with neither format compiled in nothing builds one,
    // and § Operations makes the empty feature set a supported build.
    #[cfg(any(feature = "fits", feature = "xisf"))]
    pub(crate) fn new(
        own: std::sync::Arc<[Keyword]>,
        inherited: std::sync::Arc<[Keyword]>,
    ) -> KeywordSet {
        KeywordSet { own, inherited }
    }

    /// The reported list, in stored order.
    pub(crate) fn view(&self) -> Keywords<'_> {
        Keywords {
            own: &self.own,
            inherited: &self.inherited,
        }
    }
}

/// Printed as the one list it reports, so a `Header`'s `Debug` does not expose the split.
impl std::fmt::Debug for KeywordSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.view(), f)
    }
}

/// The keywords that apply to one image, in stored order — what [`crate::Header::keywords`]
/// reports.
///
/// A borrowed view rather than a slice, and that is a memory bound rather than a style: the
/// concatenated list is not stored anywhere, because storing it costs every image extension a
/// copy of the whole primary header. See `KeywordSet` for the arithmetic.
///
/// It reads like a slice — [`len`](Self::len), [`get`](Self::get), indexing, and `iter()` — and
/// the elements it yields borrow from the `Header`, not from the view, so a view is free to be
/// copied, passed and dropped.
#[derive(Clone, Copy)]
pub struct Keywords<'a> {
    own: &'a [Keyword],
    inherited: &'a [Keyword],
}

impl<'a> Keywords<'a> {
    /// How many keywords are reported.
    pub fn len(self) -> usize {
        self.own.len() + self.inherited.len()
    }

    /// Whether the image reports none.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The keyword at `index` in stored order.
    pub fn get(self, index: usize) -> Option<&'a Keyword> {
        match index.checked_sub(self.own.len()) {
            None => self.own.get(index),
            Some(rest) => self.inherited.get(rest),
        }
    }

    /// The keywords, in stored order.
    pub fn iter(self) -> KeywordIter<'a> {
        self.own.iter().chain(self.inherited)
    }
}

/// What [`Keywords::iter`] returns: the two pieces, back to back.
pub type KeywordIter<'a> =
    std::iter::Chain<std::slice::Iter<'a, Keyword>, std::slice::Iter<'a, Keyword>>;

impl<'a> IntoIterator for Keywords<'a> {
    type Item = &'a Keyword;
    type IntoIter = KeywordIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::ops::Index<usize> for Keywords<'_> {
    type Output = Keyword;

    fn index(&self, index: usize) -> &Keyword {
        self.get(index)
            .unwrap_or_else(|| panic!("keyword index {index} is past the reported {}", self.len()))
    }
}

/// Printed as one list, so the split is invisible from outside exactly as it is from the
/// accessors.
impl std::fmt::Debug for Keywords<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Which of XISF's scopes a property was found in.
///
/// The specification has three (§11.4) — root, `Image` and `Metadata` — but only two are
/// ever *reported against an image*: an image's own child properties, and root-level
/// `Metadata`-scope ones. A root-level property reaching an image through a `Reference` is
/// tagged with the scope of the element it attaches to, and an **unreferenced** root-level
/// property is attached to no image and is not reported at all, so no path produces a root
/// scope. See `docs/implementation-decisions.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PropertyScope {
    /// A `Property` child of the selected `<Image>`, or one reaching it by `Reference`.
    Image,
    /// A root-level `Property` inside the mandatory `<Metadata>` element.
    Metadata,
}

/// A property's declared type, from the specification's own closed vocabulary.
///
/// §11.1.1 makes the `type` attribute the name of an XISF property type, and §8.4.4 has
/// already closed that vocabulary, so a decoder modelling it as free text is declining to
/// read a decision the standard made. [`PropertyType::Other`] is what keeps the report
/// lossless for a name this version does not recognize.
///
/// Tables 3 through 8 give many names an **alternate spelling** — `Byte` for `UInt8`,
/// `Complex` for `Complex64`, `Vector` for `F64Vector`, `Matrix` for `F64Matrix` and the
/// rest. Both spellings name one type and both resolve to the one variant: which spelling a
/// writer chose says nothing about the value, and the value is reported verbatim regardless.
///
/// There is **no `Table` variant**: §11.1 excludes table properties from `Property`
/// altogether, so an element spelling `type="Table"` is non-conforming and lands in `Other`
/// like any other unrecognized name.
///
/// Adding a variant later reclassifies files that used to land in `Other`, so `Other` is a
/// value to report and to log — never a stable thing for a consumer to match on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[allow(missing_docs)] // one line per specification type name would say only the name again
pub enum PropertyType {
    // §8.4.4.1 Table 3 — scalars
    Boolean,
    Int8,
    /// Also spelled `Byte`.
    UInt8,
    /// Also spelled `Short`.
    Int16,
    /// Also spelled `UShort`.
    UInt16,
    /// Also spelled `Int`.
    Int32,
    /// Also spelled `UInt`.
    UInt32,
    Int64,
    UInt64,
    Int128,
    UInt128,
    /// Also spelled `Float`.
    Float32,
    /// Also spelled `Double`.
    Float64,
    /// Also spelled `Quad`.
    Float128,

    // §8.4.4.2 Table 4 — complex
    Complex32,
    /// Also spelled `Complex`.
    Complex64,
    Complex128,

    // §8.4.4.3 Table 5, §8.4.4.4 Table 6
    String,
    TimePoint,

    // §8.4.4.5 Table 7 — vectors
    I8Vector,
    /// Also spelled `ByteArray`.
    UI8Vector,
    I16Vector,
    UI16Vector,
    /// Also spelled `IVector`.
    I32Vector,
    /// Also spelled `UIVector`.
    UI32Vector,
    I64Vector,
    UI64Vector,
    I128Vector,
    UI128Vector,
    F32Vector,
    /// Also spelled `Vector`.
    F64Vector,
    F128Vector,
    C32Vector,
    C64Vector,
    C128Vector,

    // §8.4.4.6 Table 8 — matrices
    I8Matrix,
    /// Also spelled `ByteMatrix`.
    UI8Matrix,
    I16Matrix,
    UI16Matrix,
    /// Also spelled `IMatrix`.
    I32Matrix,
    /// Also spelled `UIMatrix`.
    UI32Matrix,
    I64Matrix,
    UI64Matrix,
    I128Matrix,
    UI128Matrix,
    F32Matrix,
    /// Also spelled `Matrix`.
    F64Matrix,
    F128Matrix,
    C32Matrix,
    C64Matrix,
    C128Matrix,

    /// A type name this version does not recognize, carrying the file's own text.
    ///
    /// Shared rather than owned, for the reason [`Property`] gives: this text is an XML
    /// attribute value, and one `<Property>` reached by many `<Reference>` elements carries it
    /// once per occurrence.
    Other(std::sync::Arc<str>),
}

impl PropertyType {
    /// Classify a `type` attribute's text.
    ///
    /// Both a primary specification name and its alternate spelling resolve to the same
    /// variant; anything else is preserved verbatim in [`PropertyType::Other`].
    pub fn classify(name: &str) -> PropertyType {
        use PropertyType as P;
        match name {
            "Boolean" => P::Boolean,
            "Int8" => P::Int8,
            "UInt8" | "Byte" => P::UInt8,
            "Int16" | "Short" => P::Int16,
            "UInt16" | "UShort" => P::UInt16,
            "Int32" | "Int" => P::Int32,
            "UInt32" | "UInt" => P::UInt32,
            "Int64" => P::Int64,
            "UInt64" => P::UInt64,
            "Int128" => P::Int128,
            "UInt128" => P::UInt128,
            "Float32" | "Float" => P::Float32,
            "Float64" | "Double" => P::Float64,
            "Float128" | "Quad" => P::Float128,

            "Complex32" => P::Complex32,
            "Complex64" | "Complex" => P::Complex64,
            "Complex128" => P::Complex128,

            "String" => P::String,
            "TimePoint" => P::TimePoint,

            "I8Vector" => P::I8Vector,
            "UI8Vector" | "ByteArray" => P::UI8Vector,
            "I16Vector" => P::I16Vector,
            "UI16Vector" => P::UI16Vector,
            "I32Vector" | "IVector" => P::I32Vector,
            "UI32Vector" | "UIVector" => P::UI32Vector,
            "I64Vector" => P::I64Vector,
            "UI64Vector" => P::UI64Vector,
            "I128Vector" => P::I128Vector,
            "UI128Vector" => P::UI128Vector,
            "F32Vector" => P::F32Vector,
            "F64Vector" | "Vector" => P::F64Vector,
            "F128Vector" => P::F128Vector,
            "C32Vector" => P::C32Vector,
            "C64Vector" => P::C64Vector,
            "C128Vector" => P::C128Vector,

            "I8Matrix" => P::I8Matrix,
            "UI8Matrix" | "ByteMatrix" => P::UI8Matrix,
            "I16Matrix" => P::I16Matrix,
            "UI16Matrix" => P::UI16Matrix,
            "I32Matrix" | "IMatrix" => P::I32Matrix,
            "UI32Matrix" | "UIMatrix" => P::UI32Matrix,
            "I64Matrix" => P::I64Matrix,
            "UI64Matrix" => P::UI64Matrix,
            "I128Matrix" => P::I128Matrix,
            "UI128Matrix" => P::UI128Matrix,
            "F32Matrix" => P::F32Matrix,
            "F64Matrix" | "Matrix" => P::F64Matrix,
            "F128Matrix" => P::F128Matrix,
            "C32Matrix" => P::C32Matrix,
            "C64Matrix" => P::C64Matrix,
            "C128Matrix" => P::C128Matrix,

            other => P::Other(std::sync::Arc::from(other)),
        }
    }
}

/// A property's value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PropertyValue {
    /// Verbatim text — the `value` attribute or the element's character data, after XML
    /// entity unescaping and never parsed per the declared [`PropertyType`]. Re-rendering a
    /// number through a formatter can lose digits, and the consumer is the one that parses;
    /// `type` is reported so it can.
    ///
    /// `Arc<str>` rather than `String`, and the one place in this crate where sharing is
    /// visible from outside. It is the largest of the multiplied texts: a `value` at exactly
    /// the `Attribute value length` cap, on a root `<Property>` reached by 4096 in-image
    /// `<Reference>` elements, is 4 GB of copies from a 4 MB header. A pattern match spells
    /// `PropertyValue::Text(text)` and gets an `Arc<str>`, which derefs to `&str` — see
    /// § Deliberate divergences from prior art.
    Text(std::sync::Arc<str>),
    /// The value lives in a data block this version does not read.
    ///
    /// Reported rather than dropped, so a consumer can tell "the file does not carry
    /// `Processing:History`" from "it does and this version cannot read it". On real
    /// PixInsight output these carry the entire astrometric solution.
    Unavailable,
}

/// One XISF `Property`, reported as a tuple rather than as a bare string.
///
/// Dropping `type` would leave a consumer unable to tell `Observation:Time:Start` as a
/// `TimePoint` from the same identifier spelled as a `Float64` or a `String`, which is the
/// whole reason a consumer pins that identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    // Every text here is shared, not owned. One root-level `<Property>` reached by N in-image
    // `<Reference>` elements is N occurrences of the same element, and § XISF decisions
    // requires each to be reported: copying the texts per occurrence let 4096 references to
    // one 48 KB property retain 199 MB from a 130 KB header, which is invariant I5's
    // unbounded-allocation clause on the header-only path. The accessors return `&str` either
    // way, so only `PropertyValue::Text` is visible from outside; see `Keyword`'s buffer and
    // `Header::image_id` for the same reasoning.
    pub(crate) id: std::sync::Arc<str>,
    pub(crate) property_type: PropertyType,
    pub(crate) value: PropertyValue,
    pub(crate) format: Option<std::sync::Arc<str>>,
    pub(crate) comment: Option<std::sync::Arc<str>>,
    pub(crate) scope: PropertyScope,
}

impl Property {
    /// The identifier, verbatim and never validated as a token — a space-bearing id such as
    /// `"Instrument: colorFlag"` has been reported in the wild, and rejecting it would
    /// reject real files.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The declared type.
    pub fn property_type(&self) -> &PropertyType {
        &self.property_type
    }

    /// The value.
    pub fn value(&self) -> &PropertyValue {
        &self.value
    }

    /// The optional `format` attribute (§11.1.2).
    pub fn format(&self) -> Option<&str> {
        self.format.as_deref()
    }

    /// The optional `comment` attribute (§11.1.2).
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// Which scope it was found in.
    pub fn scope(&self) -> PropertyScope {
        self.scope
    }
}

/// The properties reported against one image, held as the pieces document order splits them
/// into rather than as one merged list.
///
/// §11.4's root `<Metadata>` properties apply to **every** image in the unit, so a merged list
/// per image is a list per image whose length is the whole document's. `XML element count` is
/// 100 000 and `Images per source` is 256, and nothing bounds their product: 40 000 root
/// properties beside 256 images that each add one property of their own allocated 3.2 GB and
/// retained 2.2 GB from a 1.9 MB header, which is invariant I5's unbounded-allocation clause on
/// the header-only path. Sharing the built `Property` values does not touch it — their texts are
/// already `Arc<str>`, so what multiplies is the merge itself.
///
/// It splits because `Doc` assigns node indices in document order and an element's subtree is a
/// contiguous run of them. An image's own properties are all nodes **inside that image's
/// subtree** — its `Property` children, and the `Reference` children standing in for root-level
/// ones, each reported at the position of the `Reference` — while every root `Metadata` property
/// is reported at the position of a child of a root-level `<Metadata>` element, which is a
/// subtree disjoint from the image's. Disjoint runs cannot interleave, so the root list splits
/// at one index: the merged document order is `metadata[..split]`, then `own`, then
/// `metadata[split..]`, and [`Properties`] is that concatenation borrowed rather than built.
///
/// The split is `metadata.partition_point(|position| position < image_node)`, which is why an
/// image nested inside a `<Metadata>` element and reached by a root-level `<Reference>` — the
/// one shape where the two elements are not siblings — still splits correctly: the metadata
/// properties are siblings of that image's subtree, so each still falls wholly before or wholly
/// after it.
#[derive(Clone, Default)]
pub(crate) struct PropertySet {
    /// Root `<Metadata>` properties (§11.4), in document order. One list per document, shared
    /// by every image in it.
    metadata: std::sync::Arc<[Property]>,
    /// The image's own, in document order.
    own: std::sync::Arc<[Property]>,
    /// How many of `metadata` precede the image element.
    split: usize,
}

impl PropertySet {
    /// Built by the XISF walk alone: a FITS header reports no property, and takes
    /// `Default::default`.
    #[cfg(feature = "xisf")]
    pub(crate) fn new(
        metadata: std::sync::Arc<[Property]>,
        own: std::sync::Arc<[Property]>,
        split: usize,
    ) -> Self {
        debug_assert!(
            split <= metadata.len(),
            "the split is an index into the root list"
        );
        Self {
            metadata,
            own,
            split,
        }
    }

    /// The merged report, in document order.
    pub(crate) fn view(&self) -> Properties<'_> {
        let (before, after) = self.metadata.split_at(self.split);
        Properties {
            before,
            own: &self.own,
            after,
        }
    }
}

/// Printed as the one list it reports, so a `Header`'s `Debug` does not expose the split.
impl std::fmt::Debug for PropertySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.view(), f)
    }
}

/// The properties that apply to one image, in document order — what [`crate::Header::properties`]
/// reports.
///
/// A borrowed view rather than a slice, and that is a memory bound rather than a style: the
/// merged list is not stored anywhere, because storing it costs every image a copy of the whole
/// root `<Metadata>` list. See `PropertySet` for the arithmetic and for why the order a view can
/// serve is the full merge and not an approximation of it.
///
/// It reads like a slice — [`len`](Self::len), [`get`](Self::get), indexing, and `iter()` — and
/// the elements it yields borrow from the `Header`, not from the view, so a view is free to be
/// copied, passed and dropped.
#[derive(Clone, Copy)]
pub struct Properties<'a> {
    before: &'a [Property],
    own: &'a [Property],
    after: &'a [Property],
}

impl<'a> Properties<'a> {
    /// How many properties are reported.
    pub fn len(self) -> usize {
        self.before.len() + self.own.len() + self.after.len()
    }

    /// Whether the image reports none — always true for FITS.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The property at `index` in document order.
    pub fn get(self, index: usize) -> Option<&'a Property> {
        match index.checked_sub(self.before.len()) {
            None => self.before.get(index),
            Some(rest) => match rest.checked_sub(self.own.len()) {
                None => self.own.get(rest),
                Some(rest) => self.after.get(rest),
            },
        }
    }

    /// The properties, in document order.
    pub fn iter(self) -> PropertyIter<'a> {
        self.before.iter().chain(self.own).chain(self.after)
    }
}

/// What [`Properties::iter`] returns: the three pieces, back to back.
pub type PropertyIter<'a> = std::iter::Chain<
    std::iter::Chain<std::slice::Iter<'a, Property>, std::slice::Iter<'a, Property>>,
    std::slice::Iter<'a, Property>,
>;

impl<'a> IntoIterator for Properties<'a> {
    type Item = &'a Property;
    type IntoIter = PropertyIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::ops::Index<usize> for Properties<'_> {
    type Output = Property;

    fn index(&self, index: usize) -> &Property {
        self.get(index)
            .unwrap_or_else(|| panic!("property index {index} is past the reported {}", self.len()))
    }
}

/// Printed as one list, so the split is invisible from outside exactly as it is from the
/// accessors.
impl std::fmt::Debug for Properties<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// An XISF `ColorFilterArray` (§11.10): the mosaic description, reported and never applied.
///
/// Left to the ignore-unknown-elements rule an XISF one-shot-colour frame would decode to
/// one channel with its CFA description silently dropped — no keyword lookup reaches an XML
/// element. FITS carries the same information in convention keywords (`BAYERPAT`,
/// `XBAYROFF`, `YBAYROFF`), which fall out of ordinary keyword reporting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cfa {
    // Shared for the reason `Header::image_id` is: a mosaic pattern is an attribute value, and
    // an image reached by many references carries it once per occurrence.
    pub(crate) pattern: std::sync::Arc<str>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) name: Option<std::sync::Arc<str>>,
}

impl Cfa {
    /// The pattern string (§11.10.1), verbatim. Its characters are drawn from Table 15 —
    /// `0`, `R`, `G`, `B`, `W`, `C`, `M`, `Y`.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The pattern's width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The pattern's height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The optional descriptive name (§11.10.2).
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// The unit an XISF `Resolution` is expressed in (§11.11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ResolutionUnit {
    /// Pixels per inch — the specification's default when the attribute is absent.
    #[default]
    Inch,
    /// Pixels per centimetre.
    Centimetre,
}

/// An XISF `Resolution` (§11.11), reported and never applied.
///
/// [`Resolution::default()`] is the specification's own default for an image carrying no
/// such element: 72.0 pixels per inch in both directions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resolution {
    pub(crate) horizontal: f64,
    pub(crate) vertical: f64,
    pub(crate) unit: ResolutionUnit,
}

impl Default for Resolution {
    fn default() -> Self {
        Resolution {
            horizontal: 72.0,
            vertical: 72.0,
            unit: ResolutionUnit::Inch,
        }
    }
}

impl Resolution {
    /// Horizontal resolution, in [`Resolution::unit`].
    pub fn horizontal(&self) -> f64 {
        self.horizontal
    }

    /// Vertical resolution, in [`Resolution::unit`].
    pub fn vertical(&self) -> f64 {
        self.vertical
    }

    /// The unit both figures are expressed in.
    pub fn unit(&self) -> ResolutionUnit {
        self.unit
    }
}

/// One channel's worth of a display function's parameters (§11.9.1).
///
/// Each of the five mandatory attributes is written `"v_RK:v_G:v_B:v_L"` — red/gray, green,
/// blue, lightness.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayChannels {
    /// The red, or gray, component.
    pub red_gray: f64,
    /// The green component.
    pub green: f64,
    /// The blue component.
    pub blue: f64,
    /// The lightness component.
    pub lightness: f64,
}

impl DisplayChannels {
    const fn uniform(v: f64) -> Self {
        DisplayChannels {
            red_gray: v,
            green: v,
            blue: v,
            lightness: v,
        }
    }
}

/// An XISF `DisplayFunction` (§11.9), reported on report-don't-interpret grounds alone: it
/// is metadata the file states and no consumer can recover otherwise. Nothing here is
/// applied to a sample.
///
/// [`DisplayFunction::default()`] is the identity display function the specification names
/// for an image carrying no such element. Its literal parameter values are given in §11.9
/// only as Equation \[23\], which the local converted copy of the specification strips to an
/// empty image reference; they are reconstructed here from the surrounding prose, which
/// states that a midtones balance of 0.5 defines a linear function, and from the midtones
/// transfer function's ordinary identity — midtones 0.5, shadows 0, highlights 1, low range
/// 0, high range 1, in every channel. Recorded in `docs/implementation-decisions.md`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayFunction {
    pub(crate) midtones: DisplayChannels,
    pub(crate) shadows: DisplayChannels,
    pub(crate) highlights: DisplayChannels,
    pub(crate) low_range: DisplayChannels,
    pub(crate) high_range: DisplayChannels,
}

impl Default for DisplayFunction {
    fn default() -> Self {
        DisplayFunction {
            midtones: DisplayChannels::uniform(0.5),
            shadows: DisplayChannels::uniform(0.0),
            highlights: DisplayChannels::uniform(1.0),
            low_range: DisplayChannels::uniform(0.0),
            high_range: DisplayChannels::uniform(1.0),
        }
    }
}

impl DisplayFunction {
    /// The `m` attribute — midtones balance.
    pub fn midtones(&self) -> DisplayChannels {
        self.midtones
    }

    /// The `s` attribute — shadows clipping point.
    pub fn shadows(&self) -> DisplayChannels {
        self.shadows
    }

    /// The `h` attribute — highlights clipping point.
    pub fn highlights(&self) -> DisplayChannels {
        self.highlights
    }

    /// The `l` attribute — low dynamic range expansion bound.
    pub fn low_range(&self) -> DisplayChannels {
        self.low_range
    }

    /// The `r` attribute — high dynamic range expansion bound.
    pub fn high_range(&self) -> DisplayChannels {
        self.high_range
    }
}
