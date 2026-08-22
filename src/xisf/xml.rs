//! The XISF XML header: a guarded pull parse into a flat node arena.
//!
//! `quick-xml` gives a Rust implementation none of what Go's standard XML parser gives a Go
//! implementation for free, so every guard here is explicit. The header is fully buffered anyway —
//! its declared length is read incrementally under a cap rather than pre-allocated — so
//! building an arena costs nothing extra and is what makes `Reference` resolution a free
//! second pass.
//!
//! Two rules about text pull in opposite directions and are both honoured. §10.3 says white
//! space "is irrelevant and *must* be ignored" for Base64 and Base16 block encodings, while
//! §11.1.6 says a `String` property's character data is white space "a compliant decoder must
//! preserve". Both surfaces are read by this one parser, so `quick-xml`'s obvious
//! `trim_text(true)` would silently corrupt every string property. Text is therefore
//! preserved here and stripped at the two decode sites instead.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};
use crate::limits::Limits;

/// A half-open range of [`Doc::arena`], standing in for an owned `String`.
///
/// Every element name and every attribute name and value is a small string, and a header is
/// mostly small strings: 49 000 `<FITSKeyword>` elements carry 294 000 of them. One `String`
/// apiece is 294 000 heap allocations, each rounded up to the allocator's 16-byte minimum,
/// plus 24 bytes of pointer, length and capacity carried in the node — which put a 2.4 MB
/// header's parse over the fuzz oracle's allocation bound. Interned, the same strings are one
/// buffer and an 8-byte span; `tests/header_alloc.rs` is what holds that.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Span {
    start: u32,
    end: u32,
}

impl Span {
    fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// One XML element, flattened into an arena.
#[derive(Debug)]
pub(crate) struct Node {
    /// The element's **local** name, namespace prefix stripped, when the element resolves
    /// into the XISF namespace or into no namespace at all. An element bound to any other
    /// namespace is never matched by local name, so its identity is not thrown away — it is
    /// kept in Clark notation (`{uri}local`) instead, which cannot collide with a bare core
    /// name like `"Image"` because `{` is not a legal XML name character.
    pub(crate) name: Span,
    /// Attributes, with XML entity references already resolved — "verbatim" means after
    /// unescaping. A consumer comparing a keyword value against a string should not have to
    /// know how the writer chose to escape it.
    pub(crate) attrs: Vec<(Span, Span)>,
    /// Character data, assembled across CDATA and entity boundaries, with white space
    /// preserved exactly.
    ///
    /// Owned, unlike the names and values beside it, and deliberately so: text is *assembled*
    /// across CDATA and entity boundaries, so interning it would mean either interleaving
    /// fragments of one element's text with another element's names or copying the whole
    /// thing again at the end. It is also not the cost — only the elements that carry text
    /// pay for it, and a header of empty elements pays nothing.
    pub(crate) text: String,
    /// Child element indices, in document order.
    pub(crate) children: Vec<usize>,
}

/// A parsed XISF header.
#[derive(Debug)]
pub(crate) struct Doc {
    /// Every element name and every attribute name and value, concatenated. The nodes hold
    /// [`Span`]s into it.
    ///
    /// **Grown naturally, never pre-allocated from the declared header length** — that
    /// declared size is unvalidated, and sizing a buffer from it is the one thing the whole
    /// header path is arranged to avoid (invariant I4).
    arena: String,
    pub(crate) nodes: Vec<Node>,
    /// The `<xisf>` element.
    pub(crate) root: usize,
    uids: HashMap<String, usize>,
}

impl Doc {
    /// One attribute's value.
    pub(crate) fn attr(&self, node: usize, name: &str) -> Option<&str> {
        let node = self.nodes.get(node)?;
        node.attrs
            .iter()
            .find(|(k, _)| self.span(*k) == name)
            .map(|(_, v)| self.span(*v))
    }

    /// This element's child indices.
    pub(crate) fn children(&self, node: usize) -> &[usize] {
        self.nodes
            .get(node)
            .map(|n| n.children.as_slice())
            .unwrap_or(&[])
    }

    /// This element's local name.
    pub(crate) fn name(&self, node: usize) -> &str {
        self.nodes
            .get(node)
            .map(|n| self.span(n.name))
            .unwrap_or("")
    }

    /// The text one span stands for.
    fn span(&self, span: Span) -> &str {
        &self.arena[span.range()]
    }

    /// This element's assembled character data.
    pub(crate) fn text(&self, node: usize) -> &str {
        self.nodes.get(node).map(|n| n.text.as_str()).unwrap_or("")
    }

    /// Resolve one `Reference`, exactly one hop.
    ///
    /// Forward references are legal — §11.13 requires only that the target be defined in the
    /// same unit, and the specification's own examples define the target *after* the
    /// `Reference` — so a one-pass backward-only resolver would silently drop metadata on
    /// conforming files. Resolution needs no cycle detection: §11.13 makes `Reference` the
    /// only core element that cannot itself carry a `uid`, so chains are not expressible.
    pub(crate) fn resolve(&self, node: usize) -> Option<usize> {
        let uid = self.attr(node, "ref")?;
        let target = *self.uids.get(uid)?;
        // A Reference resolving to another Reference is not expressible, but a hostile file
        // can still spell `uid` on one; refuse the hop rather than following it.
        if self.name(target) == "Reference" {
            return None;
        }
        Some(target)
    }
}

/// The namespace §9.5 says the root *should* carry.
///
/// "Should", not "must", so a header omitting it is conforming and its elements are matched
/// by local name unconditionally. A root in some **other** namespace is a different thing
/// entirely and is `Malformed`: matching its local names would decode a foreign document as
/// though it were XISF, and refusing to match them would walk zero images on a file that
/// plainly is not one — the silent loss this design refuses everywhere.
pub(crate) const XISF_NAMESPACE: &str = "http://www.pixinsight.com/xisf";

/// Elements this crate reports that §11.13 permits at the root, and so the only targets a
/// root-level `Reference` may resolve to. A `Reference` to anything else is ignored.
pub(crate) const REFERENCEABLE: [&str; 6] = [
    "FITSKeyword",
    "Property",
    "ColorFilterArray",
    "Resolution",
    "DisplayFunction",
    "Image",
];

/// Parse an XISF XML header under the caps.
///
/// Parsing stops at `</xisf>`. A signed unit places its `<Signature>` element *after* that
/// (§9.5), so the header buffer has two roots and is not a well-formed standalone document —
/// which is fine, and is what makes a signed unit decode normally even though signature
/// verification is out of scope.
pub(crate) fn parse(bytes: &[u8], limits: &Limits) -> Result<Doc> {
    let mut reader = Reader::from_reader(bytes);
    // Text is preserved rather than trimmed: see the module documentation.
    reader.config_mut().trim_text(false);
    // Emit `<Image .../>` as a Start/End pair, so one arm handles both spellings and an
    // empty element cannot be pushed onto the stack without a matching pop.
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_end_names = true;
    // A signed unit places its `<Signature>` after `</xisf>` (§9.5), so the buffer has two
    // roots; the walk below stops at the first root's end and never reaches the second.
    reader.config_mut().allow_unmatched_ends = false;

    let mut arena = String::new();
    // One set for the whole document, cleared per element. See `read_attributes`.
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Sized from the `<` count rather than grown by doubling. A header of many small elements
    // is the worst shape this parser meets — per-node overhead is fixed while the source text
    // it is charged against shrinks toward four bytes — and *cumulative* allocation, which is
    // what the § Fuzzing oracle measures, counts every reallocation copy on the way. One
    // linear scan removes all of them.
    //
    // Not an allocation from a declared size (invariant I4): the figure comes from bytes the
    // source actually produced, and it is clamped to the element cap that bounds the walk
    // anyway, so a header cannot ask for more than it is already allowed to contain.
    let expected = bytes
        .iter()
        .filter(|b| **b == b'<')
        .count()
        .min(limits.xml_elements as usize);
    let mut nodes: Vec<Node> = Vec::with_capacity(expected);
    let mut stack: Vec<usize> = Vec::new();
    // The prefix→URI bindings each open element on `stack` declared on its own start tag,
    // index-aligned with `stack`. Resolving a prefix walks this from the top down, which is
    // exactly namespace scoping: an inner redeclaration shadows an outer one.
    let mut scopes: Vec<Bindings> = Vec::new();
    let mut root: Option<usize> = None;
    let mut buf = Vec::new();
    let mut elements: u64 = 0;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::malformed(format!("XISF header is not well-formed XML: {e}")))?;

        match event {
            Event::DocType(_) => {
                // Rejected outright. No legitimate XISF header carries one, and refusing it
                // removes DTDs, entity declarations and XXE as a category in one rule.
                return Err(Error::malformed(
                    "XISF header carries a DOCTYPE declaration, which no conforming header \
                     needs and which this crate refuses outright",
                ));
            }
            Event::Start(start) => {
                elements += 1;
                if elements > limits.xml_elements {
                    return Err(Error::limit(format!(
                        "total element count: the header declares more than the {} elements \
                         the cap allows",
                        limits.xml_elements
                    )));
                }
                if stack.len() as u64 >= u64::from(limits.xml_depth) {
                    return Err(Error::limit(format!(
                        "element nesting depth: the header nests past the {} level cap",
                        limits.xml_depth
                    )));
                }

                let qname = start.name();
                let qname = qname.as_ref();
                let own_bindings = declared_bindings(&start, limits);
                // Bindings declared on this very start tag apply to the tag itself (XML
                // Namespaces §5.3), so `own_bindings` is searched before `scopes`, and
                // `scopes` is empty at the root — there is nothing above it to inherit from.
                let prefix = qname_prefix(qname);
                let resolved = resolve_prefix(prefix.as_deref(), &own_bindings, &scopes);
                if stack.is_empty() {
                    check_root_namespace(prefix.as_deref(), resolved)?;
                }
                let local = local_name(qname);
                let name = if in_xisf_or_no_namespace(prefix.as_deref(), resolved) {
                    intern(&mut arena, &[&local])?
                } else {
                    // An undeclared prefix has no URI to key on, so its own text stands in:
                    // `{ev:}Image` is as distinct from `Image` as `{uri}Image` is, and `{` is
                    // not a legal XML name character, so neither can collide with a core name.
                    match resolved {
                        Some(uri) => intern(&mut arena, &["{", uri, "}", &local])?,
                        None => intern(
                            &mut arena,
                            &["{", prefix.as_deref().unwrap_or(""), ":}", &local],
                        )?,
                    }
                };
                let attrs = read_attributes(
                    &start,
                    limits,
                    &own_bindings,
                    &scopes,
                    &mut seen,
                    &mut arena,
                )?;
                let index = nodes.len();
                nodes.push(Node {
                    name,
                    attrs,
                    text: String::new(),
                    children: Vec::new(),
                });
                if let Some(&parent) = stack.last() {
                    nodes[parent].children.push(index);
                } else if root.is_none() {
                    root = Some(index);
                }
                stack.push(index);
                scopes.push(own_bindings);
            }
            Event::End(_) => {
                let closed = stack.pop();
                scopes.pop();
                if closed == root {
                    break;
                }
            }
            Event::Text(text) => {
                // Character data is assembled across CDATA and entity boundaries *before*
                // either white-space rule applies, so a `String` property split by a CDATA
                // section reads the same as one written plainly.
                let decoded = text.xml_content(XmlVersion::Explicit1_0).map_err(|e| {
                    Error::malformed(format!("XISF header text is not valid UTF-8: {e}"))
                })?;
                if let Some(&top) = stack.last() {
                    nodes[top].text.push_str(&decoded);
                }
            }
            Event::CData(data) => {
                let raw = String::from_utf8(data.to_vec())
                    .map_err(|_| Error::malformed("XISF header CDATA is not valid UTF-8"))?;
                if let Some(&top) = stack.last() {
                    nodes[top].text.push_str(&raw);
                }
            }
            Event::GeneralRef(entity) => {
                // 0.41 splits references out of `Text` into their own event, which is where
                // entity expansion is bounded: a character reference resolves to one
                // character, and a named reference resolves only against the five predefined
                // XML entities. DOCTYPE is refused above and the internal subset is skipped
                // rather than processed, so no other entity is ever *defined* -- which is
                // what makes billion-laughs inexpressible rather than merely capped.
                let resolved = resolve_entity(&entity)?;
                if let Some(&top) = stack.last() {
                    nodes[top].text.push_str(&resolved);
                }
            }
            Event::Decl(decl) => {
                // §9.5 fixes the header encoding as UTF-8. A *declared* other encoding is
                // `Unsupported` rather than `Malformed` — the file is valid, it just asks for
                // something this build cannot do, `quick-xml` being compiled without its
                // transcoding feature. Guessing would be worse than refusing. A **missing**
                // declaration is tolerated: it makes the header invalid by §9.5 and real
                // writers omit it, and nothing in decoding depends on it.
                if let Some(Ok(encoding)) = decl.encoding() {
                    let name = String::from_utf8_lossy(&encoding).into_owned();
                    if !name.eq_ignore_ascii_case("utf-8") && !name.eq_ignore_ascii_case("utf8") {
                        return Err(Error::unsupported(format!(
                            "XISF header declares the encoding {name:?}; §9.5 fixes it as UTF-8 \
                             and this build carries no transcoder"
                        )));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    let root = root.ok_or_else(|| Error::malformed("XISF header carries no root element"))?;
    if &arena[nodes[root].name.range()] != "xisf" {
        return Err(Error::malformed(format!(
            "XISF header's root element is {:?}, not xisf",
            &arena[nodes[root].name.range()]
        )));
    }

    let uids = collect_uids(&arena, &nodes);

    Ok(Doc {
        arena,
        nodes,
        root,
        uids,
    })
}

/// Index the `uid`-bearing referenceable elements, first occurrence winning.
fn collect_uids(arena: &str, nodes: &[Node]) -> HashMap<String, usize> {
    let mut uids = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if !REFERENCEABLE.contains(&&arena[node.name.range()]) {
            continue;
        }
        if let Some((_, uid)) = node.attrs.iter().find(|(k, _)| &arena[k.range()] == "uid") {
            uids.entry(arena[uid.range()].to_owned()).or_insert(index);
        }
    }
    uids
}

/// Append `parts` to the arena and hand back the span they occupy.
///
/// The arena is addressed by `u32`, which is what makes a span 8 bytes rather than 16 and is
/// the point of interning at all. Eight MiB of header cannot approach that ceiling, but
/// `xml_header_bytes` is a caller's to raise and Clark notation copies a namespace URI into
/// every element bound to it, so the conversion is checked rather than assumed: a wrapped
/// index would resolve to the wrong text silently, which is worse than any refusal.
fn intern(arena: &mut String, parts: &[&str]) -> Result<Span> {
    let start = arena_index(arena.len())?;
    for part in parts {
        arena.push_str(part);
    }
    let end = arena_index(arena.len())?;
    Ok(Span { start, end })
}

fn arena_index(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::limit(
            "XISF header names and attribute values: their total exceeds the 4 GiB the header \
             parser addresses",
        )
    })
}

/// Strip a QName down to its **local** part — the piece after the last `:`, or the whole
/// thing when there is no prefix.
///
/// Used for both halves of the namespace rule: unconditionally for the root (its namespace
/// is checked separately, by `check_root_namespace`, and a mismatch there aborts the parse
/// before any other element is reached) and for every other element once `resolve_prefix` has
/// decided the element resolves into the XISF namespace or into none.
fn local_name(qname: &[u8]) -> Cow<'_, str> {
    let local = match qname.iter().rposition(|b| *b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    };
    String::from_utf8_lossy(local)
}

/// A QName's prefix, if it has one — the piece before the last `:`.
fn qname_prefix(qname: &[u8]) -> Option<Cow<'_, str>> {
    qname
        .iter()
        .rposition(|b| *b == b':')
        .map(|i| String::from_utf8_lossy(&qname[..i]))
}

/// One element's `xmlns`/`xmlns:*` bindings, prefix (`None` for the default) to URI.
type Bindings = Vec<(Option<String>, String)>;

/// The bindings one element's start tag declares.
///
/// Bounded to the same `attributes_per_element` cap `read_attributes` enforces, and for the
/// same reason: `start.attributes()` reads lazily from the raw bytes, so an unbounded scan
/// here would let an attacker's element with an enormous attribute count be walked in full
/// before `read_attributes` ever gets to reject it. A malformed attribute is skipped rather
/// than surfaced — `read_attributes` walks the same tag right after and raises the real error.
fn declared_bindings(start: &quick_xml::events::BytesStart<'_>, limits: &Limits) -> Bindings {
    let mut out = Bindings::new();
    for (seen, attr) in start.attributes().enumerate() {
        if seen as u64 >= u64::from(limits.attributes_per_element) {
            break;
        }
        let Ok(attr) = attr else { continue };
        let key = attr.key.as_ref();
        // The prefix is decided before the URI is owned, so an attribute that is not a
        // declaration leaves this loop without allocating. Owning the value first cost one
        // heap allocation per attribute in the document, on a path most attributes leave
        // immediately.
        let declared = if key == b"xmlns" {
            Some(None)
        } else {
            key.strip_prefix(b"xmlns:".as_slice())
                .map(|pfx| Some(String::from_utf8_lossy(pfx).into_owned()))
        };
        if let Some(prefix) = declared {
            let uri = String::from_utf8_lossy(attr.value.as_ref()).into_owned();
            out.push((prefix, uri));
        }
    }
    out
}

/// Resolve a prefix (`None` for the default namespace) to the URI it is bound to at one point
/// in the walk: `own` — the bindings the current element's own start tag declares, which apply
/// to the tag itself (XML Namespaces §5.3) — take precedence, then `scopes` is searched
/// innermost-ancestor-first. `None` means no binding is in scope at all, which is the "no
/// namespace" case for an unprefixed name and — for a prefixed name whose prefix was never
/// declared — is treated the same as a foreign namespace by every caller below, i.e. as not
/// XISF's.
fn resolve_prefix<'a>(
    prefix: Option<&str>,
    own: &'a Bindings,
    scopes: &'a [Bindings],
) -> Option<&'a str> {
    let search = |scope: &'a Bindings| {
        scope
            .iter()
            .rev()
            .find(|(p, _)| p.as_deref() == prefix)
            .map(|(_, uri)| uri.as_str())
    };
    search(own)
        .or_else(|| scopes.iter().rev().find_map(search))
        // `xmlns=""` *undeclares* the default namespace (XML Namespaces §6.2) rather than
        // binding it to the empty URI, so the answer is "no namespace" — the unprefixed and
        // unbound case §9.5's "should" tolerates, not a foreign one. Read literally, the empty
        // string is a URI that is not XISF's, and an `<Image xmlns="">` would have been
        // silently skipped and a root carrying it refused.
        .filter(|uri| !uri.is_empty())
}

/// Namespace URIs are compared as XML Namespaces §2.3 compares them: exact, character for
/// character, with no normalization of case, escaping or a trailing slash.
///
/// Normalizing a trailing slash away was tried and removed. It has no support: §9.5 spells the
/// URI one way, the specification uses that spelling in all ten of its occurrences, and all
/// 1066 XISF files in the local corpus declare exactly it. Accepting `.../xisf/` as XISF would
/// be a plausible repair of a document that says something else — the thing § The organizing
/// principle refuses everywhere — and it would do it on the check that decides whether a
/// foreign document is decoded as this one.
fn same_namespace(uri: &str, other: &str) -> bool {
    uri == other
}

/// Whether an element name is matched by its bare local name.
///
/// Two unbound cases look alike after resolution and are **not** the same thing, which is the
/// distinction this takes `prefix` to make:
///
/// - **Unprefixed and unbound** — no default namespace anywhere in scope. §9.5 says the
///   namespace *should* be declared, so a header omitting it is conforming, and this is the
///   "unconditionally when it does not" half of the rule. Matched.
/// - **Prefixed, and the prefix was never declared** — not "no namespace" but namespace-ill-
///   formed XML. Treating it as the unbound case would make the foreign-element refusal
///   bypassable by *deleting* the `xmlns:` attribute, which is the opposite of a guard. Not
///   matched.
fn in_xisf_or_no_namespace(prefix: Option<&str>, resolved: Option<&str>) -> bool {
    match (prefix, resolved) {
        (None, None) => true,
        (Some(_), None) => false,
        (_, Some(uri)) => same_namespace(uri, XISF_NAMESPACE),
    }
}

/// Refuse a root element that is not in XISF's namespace.
///
/// This is the one place the rule is an outright error rather than a silent non-match: a root
/// in some *other* namespace is a document that plainly is not XISF, and matching no `Image`
/// would walk zero images — the silent loss this design refuses everywhere. An **unprefixed,
/// unbound** root is conforming per §9.5's "should" and is not this case; a root carrying a
/// prefix nobody declared is, for the reason [`in_xisf_or_no_namespace`] gives.
fn check_root_namespace(prefix: Option<&str>, resolved: Option<&str>) -> Result<()> {
    if in_xisf_or_no_namespace(prefix, resolved) {
        return Ok(());
    }
    Err(match resolved {
        Some(uri) => Error::malformed(format!(
            "XISF header's root element is bound to the namespace {uri:?}, not {XISF_NAMESPACE:?}"
        )),
        None => Error::malformed(format!(
            "XISF header's root element carries the undeclared namespace prefix {:?}",
            prefix.unwrap_or_default()
        )),
    })
}

fn read_attributes(
    start: &quick_xml::events::BytesStart<'_>,
    limits: &Limits,
    own_bindings: &Bindings,
    scopes: &[Bindings],
    // `seen` is cleared per element and reused across the document: a set allocated per start
    // tag cost more than the quadratic scan it replaced, on a header of many narrow elements.
    seen: &mut std::collections::HashSet<u64>,
    arena: &mut String,
) -> Result<Vec<(Span, Span)>> {
    let mut out: Vec<(Span, Span)> = Vec::new();
    seen.clear();
    for attr in start.attributes() {
        let attr =
            attr.map_err(|e| Error::malformed(format!("XISF header attribute is malformed: {e}")))?;
        if out.len() as u64 >= u64::from(limits.attributes_per_element) {
            return Err(Error::limit(format!(
                "attributes per element: one element carries more than the {} the cap allows",
                limits.attributes_per_element
            )));
        }
        if attr.value.len() as u64 > limits.attribute_value_bytes {
            return Err(Error::limit(format!(
                "attribute value length: one value is {} bytes, above the {} byte cap",
                attr.value.len(),
                limits.attribute_value_bytes
            )));
        }
        // Unlike elements, an *unprefixed* attribute never inherits a default namespace (XML
        // Namespaces §5.2) — it is always in no namespace, which is how every core XISF
        // attribute is written even on a fully `pi:`-prefixed element (see
        // `a_namespace_prefixed_header_decodes` in `tests/xisf_decisions.rs`). A *prefixed*
        // attribute mirrors the element rule: the prefix is stripped when it resolves into the
        // XISF namespace — `<pi:Image pi:geometry="…">` with `pi` bound to XISF's namespace is
        // spelling the same `geometry` attribute the unprefixed form is — and kept qualified
        // otherwise, so two attributes with the same local name under two distinct (or
        // undeclared) prefixes stay the distinct attributes they are rather than colliding
        // into one.
        let raw_key = attr.key.as_ref();
        let key = match qname_prefix(raw_key) {
            Some(prefix) => match resolve_prefix(Some(&prefix), own_bindings, scopes) {
                Some(uri) if same_namespace(uri, XISF_NAMESPACE) => local_name(raw_key),
                _ => String::from_utf8_lossy(raw_key),
            },
            None => String::from_utf8_lossy(raw_key),
        };
        // Hashed before interning, so the duplicate check below is a set lookup rather than a
        // scan of every key accepted so far. That scan was quadratic in the attribute count,
        // and `attributes_per_element` allows 4096: an 8 MiB header of wide elements cost four
        // seconds of CPU on the header-only path a consumer runs over untrusted bytes. Bounded
        // by the caps, so not a hang — just far more work than the input justifies.
        // Hashed through the **set's own** `RandomState`, not a `DefaultHasher`. A
        // `DefaultHasher` is SipHash keyed with zeros, so the `u64` it produces is publicly
        // computable: an attacker writing 4096 colliding attribute names per element would put
        // every one of them through the fallback scan and restore exactly the quadratic cost
        // this replaced. The set is randomly seeded per process; keying the value it stores
        // with the same seed is what makes the guard hold against a chosen input rather than
        // only against an accidental one.
        let key_hash = {
            use std::hash::BuildHasher as _;
            seen.hasher().hash_one(key.as_ref())
        };
        let key = intern(arena, &[&key])?;
        // "Verbatim" means after unescaping: a consumer comparing a keyword value against a
        // string should not have to know how the writer chose to escape it. `quick_xml` does
        // not unescape automatically, so this is a decision rather than a default. The
        // unescaped form is borrowed whenever nothing needed unescaping, so the common
        // attribute costs one `memcpy` into the arena and no allocation at all.
        let value = attr
            .normalized_value(XmlVersion::Explicit1_0)
            .map_err(|e| {
                Error::malformed(format!("XISF header attribute is not resolvable: {e}"))
            })?;
        let value = intern(arena, &[&value])?;
        // Duplicate attributes on one element are rejected rather than last-wins. The hash
        // set decides it; the scan runs only when a hash matches, so a collision costs
        // correctness nothing and speed almost nothing.
        if !seen.insert(key_hash)
            && out
                .iter()
                .any(|(k, _)| arena[k.range()] == arena[key.range()])
        {
            return Err(Error::malformed(format!(
                "XISF header element carries the attribute {:?} twice",
                &arena[key.range()]
            )));
        }
        out.push((key, value));
    }
    Ok(out)
}

/// Resolve one general or character reference.
///
/// `quick_xml`'s default resolver handles only the five predefined XML entities and does not
/// resolve recursively, and its DTD handling *skips* the internal subset rather than
/// processing it, so a declared entity is never defined. Both properties belong to the
/// dependency rather than to this crate, so a test pins them.
fn resolve_entity(entity: &quick_xml::events::BytesRef<'_>) -> Result<String> {
    if let Some(c) = entity.resolve_char_ref().map_err(|e| {
        Error::malformed(format!(
            "XISF header carries an invalid character reference: {e}"
        ))
    })? {
        return Ok(c.to_string());
    }
    let name = entity.decode().map_err(|e| {
        Error::malformed(format!("XISF header entity name is not valid UTF-8: {e}"))
    })?;
    quick_xml::escape::resolve_predefined_entity(&name)
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::malformed(format!(
                "XISF header references the entity {name:?}, which is not one of XML's five \
                 predefined entities and which no declaration can define here"
            ))
        })
}
