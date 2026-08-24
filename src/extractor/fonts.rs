//! Font width parsing, encoding, and text decoding.

use crate::glyph_names::glyph_to_char;
use crate::tounicode::FontCMaps;
use crate::types::{FontEncodingMap, FontWidthInfo, PageFontEncodings, PageFontWidths};
use log::debug;
use lopdf::{Document, Encoding, Object, ObjectId};
use std::collections::HashMap;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum CMapChoice {
    Primary,
    Remapped,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct CMapDecisionCache {
    decisions: HashMap<u32, CMapDecision>,
}

#[derive(Debug, Default, Clone)]
struct CMapDecision {
    primary_sample: String,
    remapped_sample: String,
    sample_bytes: usize,
    choice: Option<CMapChoice>,
}

impl CMapDecisionCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get_choice(&self, obj_num: u32) -> Option<CMapChoice> {
        self.decisions.get(&obj_num).and_then(|d| d.choice)
    }

    pub(crate) fn consider(
        &mut self,
        obj_num: u32,
        primary: &str,
        remapped: &str,
        bytes_len: usize,
    ) -> Option<CMapChoice> {
        const SAMPLE_TARGET_BYTES: usize = 240;

        let entry = self.decisions.entry(obj_num).or_default();
        entry.sample_bytes = entry.sample_bytes.saturating_add(bytes_len);
        entry.primary_sample.push_str(primary);
        entry.remapped_sample.push_str(remapped);

        if entry.choice.is_none() && entry.sample_bytes >= SAMPLE_TARGET_BYTES {
            let score_primary = score_text(&entry.primary_sample);
            let score_remap = score_text(&entry.remapped_sample);
            entry.choice = if score_remap > score_primary + 5 {
                Some(CMapChoice::Remapped)
            } else {
                Some(CMapChoice::Primary)
            };
        }

        entry.choice
    }
}

/// Resolve a PDF object reference to an array
pub(crate) fn resolve_array<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Vec<Object>> {
    match obj {
        Object::Array(arr) => Some(arr),
        Object::Reference(r) => {
            if let Ok(Object::Array(arr)) = doc.get_object(*r) {
                Some(arr)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Resolve a PDF object reference to a dictionary
pub(crate) fn resolve_dict<'a>(
    doc: &'a Document,
    obj: &'a Object,
) -> Option<&'a lopdf::Dictionary> {
    match obj {
        Object::Dictionary(d) => Some(d),
        Object::Reference(r) => doc.get_dictionary(*r).ok(),
        _ => None,
    }
}

/// Build font width info for all fonts on a page
pub(crate) fn build_font_widths(
    doc: &Document,
    fonts: &std::collections::BTreeMap<Vec<u8>, &lopdf::Dictionary>,
) -> PageFontWidths {
    let mut widths = PageFontWidths::new();

    for (font_name, font_dict) in fonts {
        let resource_name = String::from_utf8_lossy(font_name).to_string();

        let subtype = font_dict
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).to_string())
            .unwrap_or_default();
        let base_font = font_dict
            .get(b"BaseFont")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).to_string())
            .unwrap_or_default();
        let has_tounicode = font_dict.get(b"ToUnicode").is_ok();
        let has_descendants = font_dict.get(b"DescendantFonts").is_ok();
        let encoding_str = font_dict
            .get(b"Encoding")
            .ok()
            .map(|o| match o {
                Object::Name(n) => String::from_utf8_lossy(n).to_string(),
                Object::Reference(_) => "ref(dict)".to_string(),
                Object::Dictionary(_) => "dict".to_string(),
                _ => format!("{:?}", o),
            })
            .unwrap_or_else(|| "none".to_string());

        debug!(
            "font {:<10} sub={:<12} base={:<45} toUni={:<6} enc={:<20} cid={}",
            resource_name, subtype, base_font, has_tounicode, encoding_str, has_descendants
        );

        if let Some(info) = parse_font_widths(doc, font_dict) {
            widths.insert(resource_name, info);
        }
    }

    widths
}

/// Visual-size scale factors for Type3 fonts, keyed by resource name.
///
/// A Type3 font's glyph space maps to text space through FontMatrix, so the
/// visual height of its glyphs is `nominal_size × |matrix_y| × FontBBox
/// height`. For a well-behaved font (matrix 0.001, bbox ≈ 1000 units) that
/// factor is ≈ 1.0 and the nominal size is already right. TeX PK bitmap
/// fonts (dvips → Distiller) instead use FontMatrix [1 0 0 -1 0 0] with
/// nominal sizes like 0.12, which makes every downstream font-size heuristic
/// (drop caps, sub/superscripts, small-font tables, line heights) see
/// nonsense. Fonts without a usable FontBBox are omitted (treated as 1.0).
pub(crate) fn build_type3_scales(
    doc: &Document,
    fonts: &std::collections::BTreeMap<Vec<u8>, &lopdf::Dictionary>,
) -> HashMap<String, f32> {
    let mut scales = HashMap::new();
    for (font_name, font_dict) in fonts {
        let is_type3 = font_dict
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            .is_some_and(|n| n == b"Type3");
        if !is_type3 {
            continue;
        }
        // Array elements may themselves be indirect references per PDF
        // syntax — resolve before reading the numeric value.
        let num = |o: &Object| {
            let resolved = match o {
                Object::Reference(r) => match doc.get_object(*r) {
                    Ok(inner) => inner,
                    Err(_) => return 0.0,
                },
                other => other,
            };
            match resolved {
                Object::Integer(i) => *i as f32,
                Object::Real(r) => *r,
                _ => 0.0,
            }
        };
        let Some(matrix) = font_dict
            .get(b"FontMatrix")
            .ok()
            .and_then(|o| resolve_array(doc, o))
        else {
            continue;
        };
        let Some(bbox) = font_dict
            .get(b"FontBBox")
            .ok()
            .and_then(|o| resolve_array(doc, o))
        else {
            continue;
        };
        if matrix.len() < 4 || bbox.len() < 4 {
            continue;
        }
        let scale_y = (num(&matrix[2]).powi(2) + num(&matrix[3]).powi(2)).sqrt();
        let bbox_h = (num(&bbox[3]) - num(&bbox[1])).abs();
        let scale = bbox_h * scale_y;

        // `scale` is the glyph box measured in text-space units. For a
        // self-consistent font it lands near 1.0 — the FontMatrix is the
        // reciprocal of the glyph-space em by construction — so the Tf
        // operand is already the rendered size and must be left alone.
        // A modest deviation is normal and must NOT trigger rescaling:
        // FontBBox is the glyph bounding box, not the em box, so it is
        // routinely somewhat smaller (descender..ascender ≈ 0.7) or larger
        // (tall accents > 1.0).
        //
        // Only a wildly inconsistent font gets renormalized. dvips/PK
        // bitmap fonts declare [1 0 0 -1 0 0] with glyphs spanning
        // hundreds of units, giving scale ≈ 159 against a nominal size of
        // 0.12pt — there the declared size carries no information. The
        // band is deliberately wide so that only that class qualifies,
        // while any matrix scale (including non-standard ones like 0.005
        // with a full-em bbox, scale = 5.0) is judged on the product
        // rather than on the matrix alone.
        const CONSISTENT_LO: f32 = 0.25;
        const CONSISTENT_HI: f32 = 4.0;
        if scale.is_finite() && scale > 0.0 && !(CONSISTENT_LO..=CONSISTENT_HI).contains(&scale) {
            scales.insert(String::from_utf8_lossy(font_name).to_string(), scale);
        }
    }
    scales
}

/// The name a `TextItem` carries for its font: the `/BaseFont` family name
/// ("ABCDEF+CMMI10"), which identifies the actual face, rather than the
/// arbitrary per-page resource tag ("F2").
///
/// Exception: resource names using Distiller's CID convention (`C2_0`,
/// `C0_1`) are kept as-is — `text_utils::is_cid_font` keys on that prefix
/// for micro-gap joining, and the family name carries no CID marker to
/// replace it. This is a known, deliberate wart: `TextItem::font` is the
/// face name except for this one producer convention. The clean fix is an
/// explicit CID flag on `TextItem`, which touches its ~29 construction
/// sites; do that migration when `TextItem` next changes shape, and delete
/// this carve-out with it.
pub(crate) fn item_font_name<'a>(resource_name: &'a str, base_font: &'a str) -> &'a str {
    if crate::text_utils::is_cid_font(resource_name) {
        resource_name
    } else {
        base_font
    }
}

/// Parse font widths from a font dictionary, dispatching by Subtype
pub(crate) fn parse_font_widths(
    doc: &Document,
    font_dict: &lopdf::Dictionary,
) -> Option<FontWidthInfo> {
    // Get the font subtype
    let subtype = font_dict.get(b"Subtype").ok()?;
    let subtype_name = subtype.as_name().ok()?;

    match subtype_name {
        b"Type0" => parse_type0_widths(doc, font_dict),
        b"Type1" | b"TrueType" | b"MMType1" => parse_simple_font_widths(doc, font_dict)
            .or_else(|| base14_fallback_widths(doc, font_dict)),
        b"Type3" => parse_simple_font_widths(doc, font_dict),
        _ => None,
    }
}

/// Fallback metrics for non-embedded base-14 fonts whose dictionary omits
/// `/FirstChar`/`/Widths` (legal per the PDF spec — the reader must supply
/// standard-font metrics). Without this, every glyph advances 0 and all
/// downstream gap-based logic (space synthesis, script detection, table
/// columns) collapses — common in 1990s dvips/Distiller PDFs.
///
/// Widths are resolved per code through the font's Differences encoding when
/// present, falling back to the same single-byte decode the text extractor
/// uses (cp1252-style smart punctuation for 0x80..=0x9F, Latin-1 elsewhere) —
/// so the width of a code always matches the char we extract for it.
fn base14_fallback_widths(doc: &Document, font_dict: &lopdf::Dictionary) -> Option<FontWidthInfo> {
    let base_font = font_dict
        .get(b"BaseFont")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).to_string())?;
    if !crate::extractor::base14::is_base14_font(&base_font) {
        return None;
    }

    let enc_map = parse_font_encoding(doc, font_dict)
        .map(|r| r.map)
        .unwrap_or_default();

    let mut widths = HashMap::new();
    for code in 0u16..=255 {
        // Resolution order: Differences override, then the font's BUILT-IN
        // encoding (Symbol/ZapfDingbats glyphs live at positions unrelated
        // to cp1252 — the renderer draws α for Symbol 0x61 no matter how
        // the text decoder transliterates it, so the advance must be α's),
        // then the cp1252-style fallback used by the text decoder.
        let ch = enc_map
            .get(&(code as u8))
            .copied()
            .or_else(|| crate::extractor::base14::builtin_encoding_char(&base_font, code as u8))
            .unwrap_or_else(|| decode_single_byte_fallback_char(code as u8, true));
        if let Some(w) = crate::extractor::base14::base14_char_width(&base_font, ch) {
            widths.insert(code, w);
        }
    }
    let space_width = widths.get(&32).copied().unwrap_or(250);

    debug!(
        "  base14 fallback widths for {} ({} codes mapped)",
        base_font,
        widths.len()
    );

    Some(FontWidthInfo {
        widths,
        default_width: 500,
        space_width,
        is_cid: false,
        units_scale: 0.001,
        wmode: 0,
    })
}

/// Parse widths for simple fonts (Type1, TrueType, MMType1, Type3)
/// Reads FirstChar, LastChar, and Widths array.
/// For Type3 fonts, reads FontMatrix to determine the correct units_scale.
pub(crate) fn parse_simple_font_widths(
    doc: &Document,
    font_dict: &lopdf::Dictionary,
) -> Option<FontWidthInfo> {
    let first_char = font_dict.get(b"FirstChar").ok().and_then(|o| match o {
        Object::Integer(n) => Some(*n as u16),
        Object::Reference(r) => doc.get_object(*r).ok().and_then(|o| {
            if let Object::Integer(n) = o {
                Some(*n as u16)
            } else {
                None
            }
        }),
        _ => None,
    })?;

    let last_char = font_dict.get(b"LastChar").ok().and_then(|o| match o {
        Object::Integer(n) => Some(*n as u16),
        Object::Reference(r) => doc.get_object(*r).ok().and_then(|o| {
            if let Object::Integer(n) = o {
                Some(*n as u16)
            } else {
                None
            }
        }),
        _ => None,
    })?;

    let widths_obj = font_dict.get(b"Widths").ok()?;
    let widths_array = resolve_array(doc, widths_obj)?;

    let mut widths = HashMap::new();
    let mut space_width: u16 = 0;

    for (i, w_obj) in widths_array.iter().enumerate() {
        let code = first_char + i as u16;
        if code > last_char {
            break;
        }
        let w = match w_obj {
            Object::Integer(n) => *n as u16,
            Object::Real(n) => *n as u16,
            Object::Reference(r) => {
                if let Ok(obj) = doc.get_object(*r) {
                    match obj {
                        Object::Integer(n) => *n as u16,
                        Object::Real(n) => *n as u16,
                        _ => continue,
                    }
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        if code == 32 {
            space_width = w;
        }
        widths.insert(code, w);
    }

    // Determine units_scale: for Type3 fonts, use FontMatrix[0]; for others, use 1/1000
    let units_scale = if let Ok(fm) = font_dict.get(b"FontMatrix") {
        if let Some(arr) = resolve_array(doc, fm) {
            if !arr.is_empty() {
                match &arr[0] {
                    Object::Real(r) => r.abs(),
                    Object::Integer(i) => (*i as f32).abs(),
                    _ => 0.001,
                }
            } else {
                0.001
            }
        } else {
            0.001
        }
    } else {
        0.001 // Standard 1000-unit system
    };

    // If space width wasn't found in the table, estimate from font metrics.
    // The default of 250 is calibrated for standard 1000-unit fonts (units_scale=0.001).
    // For Type3 fonts with different coordinate systems, use average glyph width instead.
    if space_width == 0 {
        if !widths.is_empty() && (units_scale - 0.001).abs() > 0.0005 {
            // Non-standard scale: estimate space as ~45% of average glyph width
            let sum: u32 = widths.values().map(|&w| w as u32).sum();
            let avg = sum as f32 / widths.len() as f32;
            space_width = (avg * 0.45).max(1.0) as u16;
        } else {
            space_width = 250;
        }
    }

    Some(FontWidthInfo {
        widths,
        default_width: 0,
        space_width,
        is_cid: false,
        units_scale,
        wmode: 0,
    })
}

/// Parse widths for Type0 (composite/CID) fonts
/// Reads DescendantFonts → CIDFont → W array and DW value
pub(crate) fn parse_type0_widths(
    doc: &Document,
    font_dict: &lopdf::Dictionary,
) -> Option<FontWidthInfo> {
    let desc_fonts_obj = font_dict.get(b"DescendantFonts").ok()?;
    let desc_fonts = resolve_array(doc, desc_fonts_obj)?;

    if desc_fonts.is_empty() {
        return None;
    }

    // Get the first descendant font dictionary
    let cid_font_dict = resolve_dict(doc, &desc_fonts[0])?;

    // Get DW (default width)
    let default_width = cid_font_dict
        .get(b"DW")
        .ok()
        .and_then(|o| match o {
            Object::Integer(n) => Some(*n as u16),
            Object::Real(n) => Some(*n as u16),
            _ => None,
        })
        .unwrap_or(1000);

    let mut widths = HashMap::new();

    // Parse W array if present
    if let Ok(w_obj) = cid_font_dict.get(b"W") {
        if let Some(w_array) = resolve_array(doc, w_obj) {
            parse_cid_w_array(doc, w_array, &mut widths);
        }
    }

    // Try to determine space width (CID 32 or CID 3 are common for space)
    let space_width = widths
        .get(&32)
        .or_else(|| widths.get(&3))
        .copied()
        .unwrap_or(if default_width > 0 {
            default_width / 4
        } else {
            250
        });

    let wmode = font_dict
        .get(b"WMode")
        .ok()
        .and_then(|o| match o {
            Object::Integer(n) => Some(*n as u8),
            _ => None,
        })
        .unwrap_or(0);

    Some(FontWidthInfo {
        widths,
        default_width,
        space_width,
        is_cid: true,
        units_scale: 0.001, // CID fonts use standard 1000-unit system
        wmode,
    })
}

/// Parse a CID W array into widths map
/// Format: [c [w1 w2 ...]] (consecutive from c) or [c_first c_last w] (range with same width)
pub(crate) fn parse_cid_w_array(
    doc: &Document,
    w_array: &[Object],
    widths: &mut HashMap<u16, u16>,
) {
    let mut i = 0;
    let mut assigned = 0usize;
    while i < w_array.len() {
        if assigned >= crate::tounicode::MAX_CID_W_EXPANSION {
            return;
        }
        let start_cid = match &w_array[i] {
            Object::Integer(n) => *n as u16,
            Object::Real(n) => *n as u16,
            _ => {
                i += 1;
                continue;
            }
        };
        i += 1;
        if i >= w_array.len() {
            break;
        }

        // Check if next element is an array (consecutive widths) or integer (range)
        match &w_array[i] {
            Object::Array(arr) => {
                // [c [w1 w2 ...]] — consecutive widths starting at c
                for (j, w_obj) in arr.iter().enumerate() {
                    if !assign_cid_width(
                        widths,
                        start_cid.wrapping_add(j as u16),
                        w_obj,
                        &mut assigned,
                    ) {
                        return;
                    }
                }
                i += 1;
            }
            Object::Reference(r) => {
                // Could be a reference to an array
                if let Ok(Object::Array(arr)) = doc.get_object(*r) {
                    for (j, w_obj) in arr.iter().enumerate() {
                        if !assign_cid_width(
                            widths,
                            start_cid.wrapping_add(j as u16),
                            w_obj,
                            &mut assigned,
                        ) {
                            return;
                        }
                    }
                    i += 1;
                } else {
                    // Treat as c_first c_last w
                    i += 1; // skip this
                }
            }
            Object::Integer(end_cid) => {
                // [c_first c_last w] — range with uniform width
                let end = *end_cid as u16;
                i += 1;
                if i >= w_array.len() {
                    break;
                }
                let w = match &w_array[i] {
                    Object::Integer(n) => *n as u16,
                    Object::Real(n) => *n as u16,
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                if !assign_cid_width_range(widths, start_cid, end, w, &mut assigned) {
                    return;
                }
                i += 1;
            }
            Object::Real(end_cid) => {
                let end = *end_cid as u16;
                i += 1;
                if i >= w_array.len() {
                    break;
                }
                let w = match &w_array[i] {
                    Object::Integer(n) => *n as u16,
                    Object::Real(n) => *n as u16,
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                if !assign_cid_width_range(widths, start_cid, end, w, &mut assigned) {
                    return;
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
}

fn assign_cid_width(
    widths: &mut HashMap<u16, u16>,
    cid: u16,
    w_obj: &Object,
    assigned: &mut usize,
) -> bool {
    let w = match w_obj {
        Object::Integer(n) => *n as u16,
        Object::Real(n) => *n as u16,
        _ => return true,
    };
    if *assigned >= crate::tounicode::MAX_CID_W_EXPANSION {
        return false;
    }
    widths.insert(cid, w);
    *assigned += 1;
    true
}

fn assign_cid_width_range(
    widths: &mut HashMap<u16, u16>,
    start: u16,
    end: u16,
    w: u16,
    assigned: &mut usize,
) -> bool {
    if start > end {
        return true;
    }
    for cid in start..=end {
        if *assigned >= crate::tounicode::MAX_CID_W_EXPANSION {
            return false;
        }
        widths.insert(cid, w);
        *assigned += 1;
    }
    true
}

/// Compute the width of a string in text space units,
/// given raw bytes and font width info.
/// Returns width in text space units (font_units * units_scale * font_size).
///
/// `char_spacing` (Tc) is added per character and `word_spacing` (Tw) is added
/// per space character (byte 0x20), both in unscaled text-space units.
/// Per the PDF spec: tx = (w0 × Tfs + Tc + Tw_if_space) per glyph.
/// One raw (unscaled, font-unit) glyph width, keyed by its CID (2-byte CMap)
/// or raw byte code (1-byte/simple font) — the same unit
/// `ToUnicodeCMap::decode_cids_glyphs` indexes its per-code decode by, so the
/// two can be walked/zipped positionally. `is_space` marks code 32 (CID or
/// byte), which is where Tw (word spacing) applies.
pub(crate) struct RawGlyphWidth {
    pub(crate) code: u16,
    pub(crate) raw_width: f32,
    pub(crate) is_space: bool,
}

/// Per-code raw glyph widths, in font units (not yet scaled to text space).
/// This is `compute_string_width_ts`'s old inline loop, factored out so its
/// per-code step can be shared with the combined decode/width walk — the
/// loop over `bytes` now runs once for both concerns rather than twice.
fn raw_glyph_widths(bytes: &[u8], font_info: &FontWidthInfo) -> Vec<RawGlyphWidth> {
    let mut out = Vec::new();
    if font_info.is_cid {
        // 2-byte (big-endian) character codes
        let mut j = 0;
        while j + 1 < bytes.len() {
            let cid = u16::from_be_bytes([bytes[j], bytes[j + 1]]);
            let w = font_info
                .widths
                .get(&cid)
                .copied()
                .unwrap_or(font_info.default_width);
            out.push(RawGlyphWidth {
                code: cid,
                raw_width: w as f32,
                is_space: cid == 32, // CID 32 = space in most CID fonts
            });
            j += 2;
        }
    } else {
        // 1-byte character codes
        for &b in bytes {
            let code = b as u16;
            let w = font_info
                .widths
                .get(&code)
                .copied()
                .unwrap_or(font_info.default_width);
            out.push(RawGlyphWidth {
                code,
                raw_width: w as f32,
                is_space: b == 0x20,
            });
        }
    }
    out
}

/// Compute the width of a string in text space units,
/// given raw bytes and font width info.
/// Returns width in text space units (font_units * units_scale * font_size).
///
/// `char_spacing` (Tc) is added per character and `word_spacing` (Tw) is added
/// per space character (byte 0x20), both in unscaled text-space units.
/// Per the PDF spec: tx = (w0 × Tfs + Tc + Tw_if_space) per glyph.
///
/// Implemented on top of `raw_glyph_widths`, but preserves the *exact*
/// original arithmetic order (sum raw per-code widths first, scale the sum
/// once, add the Tc/Tw aggregates once) rather than distributing
/// `units_scale * font_size`/Tc/Tw into each code and summing — those are
/// two different (if mathematically equal) f32 evaluation orders, and only
/// this one is guaranteed bit-identical to the pre-refactor implementation.
pub(crate) fn compute_string_width_ts(
    bytes: &[u8],
    font_info: &FontWidthInfo,
    font_size: f32,
    char_spacing: f32,
    word_spacing: f32,
) -> f32 {
    let widths = raw_glyph_widths(bytes, font_info);
    let mut total: f32 = 0.0;
    let mut num_spaces: usize = 0;
    for w in &widths {
        total += w.raw_width;
        if w.is_space {
            num_spaces += 1;
        }
    }
    total * font_info.units_scale * font_size
        + widths.len() as f32 * char_spacing
        + num_spaces as f32 * word_spacing
}

/// Extract raw bytes from a PDF operand (String object)
pub(crate) fn get_operand_bytes(obj: &Object) -> Option<&[u8]> {
    if let Object::String(bytes, _) = obj {
        Some(bytes)
    } else {
        None
    }
}

/// Build encoding maps for all fonts on a page.
/// Returns `(encodings, has_gid_fonts)` where `has_gid_fonts` is true when
/// any font uses raw glyph ID names (gidNNNNN) that can't be decoded.
/// Gid names whose codes the font's own ToUnicode CMap maps are decodable
/// and do not set the flag (LibreOffice subsets write /gidNNNN Differences
/// names alongside a complete ToUnicode CMap).
pub(crate) fn build_font_encodings(
    doc: &Document,
    fonts: &std::collections::BTreeMap<Vec<u8>, &lopdf::Dictionary>,
    cmaps: &FontCMaps,
) -> (PageFontEncodings, bool) {
    let mut encodings = PageFontEncodings::new();
    let mut has_gid_fonts = false;

    for (font_name, font_dict) in fonts {
        let resource_name = String::from_utf8_lossy(font_name).to_string();

        if let Some(result) = parse_font_encoding(doc, font_dict) {
            if !result.gid_codes.is_empty()
                && !tounicode_maps_codes(font_dict, cmaps, &result.gid_codes)
            {
                has_gid_fonts = true;
            }
            if !result.map.is_empty() {
                encodings.insert(resource_name, result.map);
            }
        }
    }

    (encodings, has_gid_fonts)
}

/// True when the font's ToUnicode CMap maps the gid-named character codes,
/// so the Differences entries still decode through the CMap.
fn tounicode_maps_codes(font_dict: &lopdf::Dictionary, cmaps: &FontCMaps, codes: &[u8]) -> bool {
    let Some(obj_ref) = font_dict
        .get(b"ToUnicode")
        .ok()
        .and_then(|o| o.as_reference().ok())
    else {
        return false;
    };
    let Some(entry) = cmaps.get_by_obj(obj_ref.0) else {
        return false;
    };
    // At least one gid code usably mapped means the CMap addresses these
    // codes; remaining unmapped codes are subset leftovers (e.g. the
    // component glyphs of an emoji ZWJ sequence mapped whole on its first
    // code). A mapping is usable only when extraction would accept it —
    // empty or U+FFFD results are rejected there as invalid. Fonts whose
    // CMap ignores the gid codes entirely stay flagged, and the downstream
    // garbage/encoding checks still catch partial damage.
    codes.iter().any(|&code| {
        entry
            .primary
            .lookup(code as u16)
            .is_some_and(|s| !s.is_empty() && !s.contains('\u{FFFD}'))
    })
}

/// Parse font encoding from a font dictionary
pub(crate) fn parse_font_encoding(
    doc: &Document,
    font_dict: &lopdf::Dictionary,
) -> Option<EncodingResult> {
    let encoding_obj = font_dict.get(b"Encoding").ok()?;
    let base_font_name = font_dict
        .get(b"BaseFont")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).to_string());

    // Encoding can be a name or a dictionary
    match encoding_obj {
        Object::Name(_name) => {
            // Standard encoding name (e.g., MacRomanEncoding, WinAnsiEncoding)
            // For standard encodings, we can use the standard tables
            // But we still need to check for Differences
            None // Let lopdf handle standard encodings
        }
        Object::Reference(obj_ref) => {
            // Reference to encoding dictionary
            if let Ok(enc_dict) = doc.get_dictionary(*obj_ref) {
                parse_encoding_dictionary(doc, enc_dict, base_font_name.as_deref())
            } else {
                None
            }
        }
        Object::Dictionary(enc_dict) => {
            parse_encoding_dictionary(doc, enc_dict, base_font_name.as_deref())
        }
        _ => None,
    }
}

/// Result of parsing an encoding dictionary's Differences array.
pub(crate) struct EncodingResult {
    pub map: FontEncodingMap,
    /// Character codes whose glyph names match the `gidNNNNN` pattern (raw
    /// glyph IDs). These reference the original font's glyph table and are
    /// only decodable when the font's ToUnicode CMap maps the code.
    pub gid_codes: Vec<u8>,
}

/// Parse an encoding dictionary with Differences array
pub(crate) fn parse_encoding_dictionary(
    doc: &Document,
    enc_dict: &lopdf::Dictionary,
    base_font_name: Option<&str>,
) -> Option<EncodingResult> {
    let differences = enc_dict.get(b"Differences").ok()?;

    let diff_array = match differences {
        Object::Array(arr) => arr.clone(),
        Object::Reference(obj_ref) => {
            if let Ok(Object::Array(arr)) = doc.get_object(*obj_ref) {
                arr.clone()
            } else {
                return None;
            }
        }
        _ => return None,
    };

    let mut encoding_map = FontEncodingMap::new();
    let mut current_code: u8 = 0;
    let mut ligature_count = 0u32;
    let mut gid_codes: Vec<u8> = Vec::new();

    for item in diff_array {
        match item {
            Object::Integer(n) => {
                // This sets the starting code for subsequent glyph names
                current_code = n as u8;
            }
            Object::Name(name) => {
                // Map current code to glyph name -> Unicode
                let glyph_name = String::from_utf8_lossy(&name).to_string();
                let mapped_char = glyph_to_char(&glyph_name)
                    .or_else(|| private_glyph_to_char(&glyph_name, base_font_name));
                if mapped_char.is_some_and(is_ligature_char) {
                    debug!(
                        "  Differences: code=0x{:02X} glyph={:?} (ligature)",
                        current_code, glyph_name
                    );
                    ligature_count += 1;
                }
                // Detect raw glyph ID names (e.g. "gid00053") that can't be
                // mapped to Unicode without the original font's cmap table.
                if glyph_name.starts_with("gid")
                    && glyph_name.len() >= 4
                    && glyph_name[3..].chars().all(|c| c.is_ascii_digit())
                {
                    gid_codes.push(current_code);
                }
                if let Some(ch) = mapped_char {
                    encoding_map.insert(current_code, ch);
                } else {
                    debug!(
                        "  Differences: code=0x{:02X} glyph={:?} (unmapped)",
                        current_code, glyph_name
                    );
                }
                current_code = current_code.wrapping_add(1);
            }
            _ => {}
        }
    }

    if ligature_count > 0 {
        debug!(
            "  Differences: {} total entries, {} ligatures",
            encoding_map.len(),
            ligature_count
        );
    }

    if !gid_codes.is_empty() {
        debug!(
            "  Differences: {} gid-encoded glyphs (decodable only via ToUnicode)",
            gid_codes.len()
        );
    }

    Some(EncodingResult {
        map: encoding_map,
        gid_codes,
    })
}

fn private_glyph_to_char(glyph_name: &str, base_font_name: Option<&str>) -> Option<char> {
    let base_font_name = strip_subset_prefix(base_font_name?);

    // Aptos CFF subsets from Office PDFs can expose the ff ligature as /g431
    // without a ToUnicode map. Keep this font-scoped because /gNNN names are private.
    if base_font_name.eq_ignore_ascii_case("Aptos") && glyph_name == "g431" {
        Some('\u{FB00}')
    } else {
        None
    }
}

fn strip_subset_prefix(font_name: &str) -> &str {
    font_name
        .split_once('+')
        .map_or(font_name, |(_, stripped)| stripped)
}

fn is_ligature_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{FB00}' | '\u{FB01}' | '\u{FB02}' | '\u{FB03}' | '\u{FB04}'
    )
}

/// Get the CMap lookup key for an Identity-H/V CID font without ToUnicode.
/// Returns the object number used by `collect_cmaps_from_fonts` to store the CMap:
/// - FontFile2 or FontFile3 obj_num (for embedded font cmap)
/// - CIDFont dict obj_num (for predefined CIDSystemInfo-based mapping)
pub(crate) fn get_font_file2_obj_num(doc: &Document, font_dict: &lopdf::Dictionary) -> Option<u32> {
    let subtype = font_dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok());

    // Type0 (CID) fonts
    if subtype == Some(b"Type0") {
        let encoding = font_dict.get(b"Encoding").ok()?.as_name().ok()?;
        if encoding != b"Identity-H" && encoding != b"Identity-V" {
            return None;
        }
        let desc_fonts_obj = font_dict.get(b"DescendantFonts").ok()?;
        let desc_fonts = resolve_array(doc, desc_fonts_obj)?;
        if desc_fonts.is_empty() {
            return None;
        }
        let cid_font_dict = resolve_dict(doc, &desc_fonts[0])?;
        let font_descriptor_obj = cid_font_dict.get(b"FontDescriptor").ok()?;
        let font_descriptor = resolve_dict(doc, font_descriptor_obj)?;

        // Try FontFile2 (TrueType), then FontFile3 (OpenType/CFF)
        if let Some(ff_ref) = font_descriptor
            .get(b"FontFile2")
            .ok()
            .and_then(|o| o.as_reference().ok())
            .or_else(|| {
                font_descriptor
                    .get(b"FontFile3")
                    .ok()
                    .and_then(|o| o.as_reference().ok())
            })
        {
            return Some(ff_ref.0);
        }

        // Fallback: use DescendantFonts[0] obj_num (for predefined CIDSystemInfo mapping)
        if let Object::Reference(r) = &desc_fonts[0] {
            return Some(r.0);
        }
        return None;
    }

    // Simple fonts: use embedded font file if available
    let font_descriptor_obj = font_dict.get(b"FontDescriptor").ok()?;
    let font_descriptor = resolve_dict(doc, font_descriptor_obj)?;
    font_descriptor
        .get(b"FontFile2")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .or_else(|| {
            font_descriptor
                .get(b"FontFile3")
                .ok()
                .and_then(|o| o.as_reference().ok())
        })
        .map(|r| r.0)
}

/// Document-scoped memo of embedded-font style flags, keyed by the
/// FontFile2/FontFile3 stream's object id. The same font program is
/// referenced from every page that uses the font, and decompressing +
/// parsing it dominates `descriptor_style_flags` — without the memo that
/// cost repeats per page whenever the descriptor leaves a flag unset
/// (the common case: regular fonts report neither italic nor bold).
#[derive(Debug, Default)]
pub(crate) struct FontStyleCache {
    by_font_file: HashMap<ObjectId, (bool, bool)>,
}

impl FontStyleCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

/// Style flags from the FontDescriptor, which survive subset fonts whose
/// BaseFont names are opaque tags ("Tc1", "ABCDEF+F1") that defeat the
/// name-based bold/italic heuristics.
///
/// Italic: `ItalicAngle` beyond a few degrees, or Flags bit 7 (Italic,
/// value 64). Bold: Flags bit 19 (ForceBold, value 1<<18). The small
/// ItalicAngle threshold skips fonts that declare a token slant.
pub(crate) fn descriptor_style_flags(
    doc: &Document,
    font_dict: &lopdf::Dictionary,
    style_cache: &mut FontStyleCache,
) -> (bool, bool) {
    let descriptor = font_dict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|obj| resolve_dict(doc, obj))
        .or_else(|| {
            // Type0 fonts hang the descriptor off DescendantFonts[0].
            let desc_fonts = font_dict.get(b"DescendantFonts").ok()?;
            let desc_fonts = resolve_array(doc, desc_fonts)?;
            let cid_font_dict = resolve_dict(doc, desc_fonts.first()?)?;
            resolve_dict(doc, cid_font_dict.get(b"FontDescriptor").ok()?)
        });
    let Some(descriptor) = descriptor else {
        return (false, false);
    };

    let italic_angle = descriptor
        .get(b"ItalicAngle")
        .ok()
        .and_then(|obj| match obj {
            Object::Integer(i) => Some(*i as f32),
            Object::Real(r) => Some(*r),
            _ => None,
        })
        .unwrap_or(0.0);
    let flags = descriptor
        .get(b"Flags")
        .ok()
        .and_then(|obj| obj.as_i64().ok())
        .unwrap_or(0);

    let mut italic = italic_angle.abs() >= 4.0 || flags & (1 << 6) != 0;
    let mut bold = flags & (1 << 18) != 0;

    // Descriptors lie: subset generators write ItalicAngle 0 for genuinely
    // italic faces. The embedded font file keeps the truth — OS/2
    // fsSelection (via `Face::is_italic`) and the post table's italicAngle.
    if !italic || !bold {
        if let Some(ff_ref) = font_file_ref(descriptor) {
            let (emb_italic, emb_bold) = *style_cache
                .by_font_file
                .entry(ff_ref)
                .or_insert_with(|| embedded_style_flags(doc, ff_ref));
            italic = italic || emb_italic;
            bold = bold || emb_bold;
        }
    }
    (italic, bold)
}

/// Style flags parsed from an embedded font program stream.
fn embedded_style_flags(doc: &Document, ff_ref: ObjectId) -> (bool, bool) {
    let Some(data) = font_file_data(doc, ff_ref) else {
        return (false, false);
    };
    if let Ok(face) = ttf_parser::Face::parse(&data, 0) {
        (
            face.is_italic() || face.italic_angle().abs() >= 4.0,
            face.is_bold(),
        )
    } else if let Some(name) = cff_font_name(&data) {
        // FontFile3 is bare CFF (no sfnt container) — ttf_parser
        // can't open it, but the CFF Name INDEX keeps the real
        // PostScript name ("XXXXXX+Amplitude-LightItalic") even
        // when the descriptor was rewritten to claim upright.
        (
            crate::text_utils::is_italic_font(&name),
            crate::text_utils::is_bold_font(&name),
        )
    } else {
        (false, false)
    }
}

/// First PostScript name from a bare CFF font's Name INDEX (CFF spec §7).
fn cff_font_name(data: &[u8]) -> Option<String> {
    // Header: major(1) minor(1) hdrSize(1) offSize(1); major must be 1.
    if data.len() < 4 || data[0] != 1 {
        return None;
    }
    let hdr_size = data[2] as usize;
    // Name INDEX: count(u16) offSize(u8) offsets[count+1] data
    let count = u16::from_be_bytes([*data.get(hdr_size)?, *data.get(hdr_size + 1)?]) as usize;
    if count == 0 {
        return None;
    }
    let off_size = *data.get(hdr_size + 2)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let read_offset = |idx: usize| -> Option<usize> {
        let at = hdr_size + 3 + idx * off_size;
        let bytes = data.get(at..at + off_size)?;
        let mut v = 0usize;
        for b in bytes {
            v = (v << 8) | *b as usize;
        }
        Some(v)
    };
    let start = read_offset(0)?;
    let end = read_offset(1)?;
    if start == 0 || end < start {
        return None;
    }
    // Offsets are 1-based from the byte before the object data.
    let objects_base = hdr_size + 3 + (count + 1) * off_size - 1;
    let name = data.get(objects_base + start..objects_base + end)?;
    Some(String::from_utf8_lossy(name).to_string())
}

/// FontFile2/FontFile3 stream reference from a FontDescriptor.
fn font_file_ref(descriptor: &lopdf::Dictionary) -> Option<ObjectId> {
    descriptor
        .get(b"FontFile2")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .or_else(|| {
            descriptor
                .get(b"FontFile3")
                .ok()
                .and_then(|o| o.as_reference().ok())
        })
}

/// Decompressed embedded font program bytes.
fn font_file_data(doc: &Document, ff_ref: ObjectId) -> Option<Vec<u8>> {
    let stream = doc
        .get_object(ff_ref)
        .and_then(lopdf::Object::as_stream)
        .ok()?;
    Some(
        stream
            .decompressed_content()
            .unwrap_or_else(|_| stream.content.clone()),
    )
}

/// One decoded glyph position within a Tj/TJ/quote-operator string operand:
/// its own decoded text, its own text-space advance width, and the CID (or
/// single byte, for a simple font) it came from.
///
/// `width_ts` is this glyph's own scaled advance
/// (`raw_font_units * units_scale * font_size`) — it deliberately does NOT
/// include the operand-level Tc/Tw (char/word spacing) terms
/// `compute_string_width_ts` adds once for the whole operand. Distributing
/// those per glyph and re-summing is not guaranteed bit-identical to that
/// function's whole-operand formula (float addition/multiplication is not
/// strictly associative/distributive over many terms), so Phase 1 keeps
/// `compute_string_width_ts`'s aggregate as the sole source of truth for
/// anything that affects text positioning (text_matrix advancement,
/// `TextItem.width`). `width_ts` here is per-glyph descriptive data for
/// later phases (e.g. pen-position tracking) — callers must not sum it to
/// reconstruct that aggregate.
///
/// `cid` is `Some(code)` for a glyph produced by a direct CID/byte lookup
/// (the normal case), and `None` for:
///   - a "filler" glyph: the 2nd+ character of a ligature CID whose
///     ToUnicode entry expands to multiple codepoints. `width_ts` is `0.0`
///     for these — the CID's one real width lives on the first character
///     only, mirroring MuPDF's own `fz_show_glyph_aux` filler-glyph
///     convention for exactly this case (`pdf-op-run.c`'s `pdf_show_char`);
///   - an "opaque" glyph from a decode path that isn't CID/byte-aligned
///     (UTF-16/UTF-8 heuristic decode, lopdf's generic `decode_text`, the
///     final single-byte catch-all, or the odd-length-CID `lookup_bytes`
///     rescue path). One glyph then carries the *entire* operand's text and
///     width, since there is no well-defined per-character split for these.
#[derive(Debug, Clone)]
pub(crate) struct GlyphDecode {
    // `text`/`cid` aren't read by any production code yet — Phase 3 (bidi
    // detection inside merge_text_items_with_glyphs) is the first
    // consumer; `#[cfg(test)]` code already reads both (see the ligature
    // regression test), just not counted by a non-test build.
    #[allow(dead_code)]
    pub(crate) text: String,
    pub(crate) width_ts: f32,
    #[allow(dead_code)]
    pub(crate) cid: Option<u16>,
    /// How many underlying CID/byte codes this glyph represents, for
    /// per-glyph Tc (char spacing) charging: 1 for a normal real glyph, 0
    /// for a ligature filler (it doesn't consume a code of its own), and
    /// the full underlying code count for an "opaque" glyph (a whole
    /// operand collapsed to one pseudo-glyph — see `cid`'s doc above).
    /// Phase 2 (pen-position tracking) uses this so
    /// `glyph_advance_ts(g, Tc, Tw)`, summed over every glyph in an
    /// operand, is provably consistent with `compute_string_width_ts`'s
    /// whole-operand aggregate (`width_ts` sums to the same raw-width
    /// term, `code_count` sums to the code count Tc is charged per, and
    /// `space_count` sums to the space count Tw is charged per) — even
    /// though, per `width_ts`'s own doc comment, summing `width_ts` alone
    /// is not bit-identical to the aggregate.
    pub(crate) code_count: u16,
    /// How many of those codes were space codes (0x20 for a simple font,
    /// CID 32 for a CID font), for per-glyph Tw (word spacing) charging —
    /// see `code_count`.
    pub(crate) space_count: u16,
    /// Page-space pen position `(x, y)` at this glyph's own draw moment —
    /// this glyph's text-rise-adjusted text matrix translation, mapped
    /// through the CTM (the exact quantity `TextItem.x`/`.y` are computed
    /// from for a whole operand, MuPDF's `trm.e`/`trm.f` per the Phase 1
    /// research). `None` here: `decode_operand_glyphs` has no access to
    /// `text_matrix`/`ctm`/`text_rise` (that state lives in the
    /// content-stream walker, not the font-decode layer) — it is always
    /// the caller's job (content_stream.rs/xobjects.rs) to fill this in
    /// glyph-by-glyph, advancing a local copy of the text matrix by each
    /// glyph's own `glyph_advance_ts`. A `Vec<GlyphDecode>` with `pen:
    /// None` throughout means positioning hasn't run on it yet.
    pub(crate) pen: Option<(f32, f32)>,
}

/// A glyph's own text-space advance, INCLUDING its share of Tc (char
/// spacing) and Tw (word spacing) — unlike `width_ts` alone (glyph width
/// only). This is the quantity a caller should add to a running pen-
/// tracking text matrix between glyphs; see `GlyphDecode::code_count`'s
/// doc comment for why summing it across an operand's glyphs reproduces
/// `compute_string_width_ts`'s aggregate.
pub(crate) fn glyph_advance_ts(g: &GlyphDecode, char_spacing: f32, word_spacing: f32) -> f32 {
    g.width_ts + g.code_count as f32 * char_spacing + g.space_count as f32 * word_spacing
}

/// Build glyphs from a code-aligned decode: `units` and `widths` must be the
/// same length and positionally aligned (one entry each per CID/byte, in
/// the same order they were walked) — the case for the primary/remapped/
/// fallback CMap paths and the Differences-map path. A unit whose decode
/// produced more than one character (a ligature CID) explodes into one real
/// glyph (the unit's own width) plus zero-width filler glyphs for the rest.
fn glyphs_from_aligned_units(
    units: &[(u16, Option<String>)],
    widths: &[RawGlyphWidth],
    units_scale: f32,
    font_size: f32,
) -> Vec<GlyphDecode> {
    let mut out = Vec::with_capacity(units.len());
    for (i, (code, text)) in units.iter().enumerate() {
        let rw = widths.get(i);
        let w = rw
            .map(|rw| rw.raw_width * units_scale * font_size)
            .unwrap_or(0.0);
        let is_space = rw.is_some_and(|rw| rw.is_space);
        let mut chars = text.as_deref().unwrap_or("").chars();
        match chars.next() {
            Some(first) => {
                out.push(GlyphDecode {
                    text: first.to_string(),
                    width_ts: w,
                    cid: Some(*code),
                    code_count: 1,
                    space_count: is_space as u16,
                    pen: None,
                });
                for extra in chars {
                    // Filler glyph: no code, no code_count/space_count — it
                    // doesn't independently consume Tc/Tw, the CID above
                    // already charged for both. See GlyphDecode's doc.
                    out.push(GlyphDecode {
                        text: extra.to_string(),
                        width_ts: 0.0,
                        cid: None,
                        code_count: 0,
                        space_count: 0,
                        pen: None,
                    });
                }
            }
            None => out.push(GlyphDecode {
                text: String::new(),
                width_ts: w,
                cid: Some(*code),
                code_count: 1,
                space_count: is_space as u16,
                pen: None,
            }),
        }
    }
    out
}

/// Build a single "opaque" glyph spanning a whole operand, for decode paths
/// that aren't CID/byte-aligned with `widths` (see `GlyphDecode` docs).
/// `code_count`/`space_count` cover ALL underlying codes at once (there's
/// no per-character split for these paths), so this one pseudo-glyph's
/// `glyph_advance_ts` alone reproduces the whole operand's aggregate.
fn opaque_glyph(
    text: String,
    widths: &[RawGlyphWidth],
    units_scale: f32,
    font_size: f32,
) -> Vec<GlyphDecode> {
    let raw_total: f32 = widths.iter().map(|w| w.raw_width).sum();
    let space_count = widths.iter().filter(|w| w.is_space).count();
    vec![GlyphDecode {
        text,
        width_ts: raw_total * units_scale * font_size,
        cid: None,
        code_count: widths.len() as u16,
        space_count: space_count as u16,
        pen: None,
    }]
}

fn units_to_string(units: &[(u16, Option<String>)]) -> String {
    units.iter().filter_map(|(_, t)| t.clone()).collect()
}

/// Combined decode: walks the operand's CIDs/bytes once, producing both the
/// decoded text (identical to the old, separate `extract_text_from_operand`)
/// and a per-glyph breakdown with widths (see `GlyphDecode`). This is the
/// real implementation — `extract_text_from_operand` is now a thin wrapper
/// over it (text decode never depends on font_size/char_spacing/
/// word_spacing, so it can call this with placeholder values for those and
/// discard the glyph half).
///
/// The full decode chain (primary/fallback/remapped CMap selection, the
/// memoized `cmap_decisions` choice, the score-based fallback-preference
/// swap, the CID-unmapped placeholder, Differences map, UTF-16/UTF-8/lopdf/
/// symbol fallbacks, and the final single-byte catch-all) is preserved
/// exactly, in the same order, with the same conditions — see
/// `extract_text_from_operand`'s original doc comment history / the Phase 1
/// research notes for the branch-by-branch trace this mirrors.
///
/// Decode text from a PDF string operand using font CMaps, encodings, and
/// fallbacks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_operand_glyphs(
    obj: &Object,
    current_font: &str,
    base_font_name: Option<&str>,
    font_cmaps: &FontCMaps,
    font_tounicode_refs: &std::collections::HashMap<String, u32>,
    inline_cmaps: &std::collections::HashMap<String, crate::tounicode::CMapEntry>,
    font_encodings: &PageFontEncodings,
    encoding_cache: &HashMap<String, Encoding<'_>>,
    cmap_decisions: &mut CMapDecisionCache,
    font_widths: &PageFontWidths,
    font_size: f32,
    char_spacing: f32,
    word_spacing: f32,
) -> (Option<String>, Vec<GlyphDecode>) {
    let font_info = font_widths.get(current_font);
    let is_type0_cid_font = font_info.is_some_and(|info| info.is_cid);
    let use_cp1252_fallback =
        should_use_cp1252_single_byte_fallback(base_font_name, is_type0_cid_font);

    let Object::String(bytes, _) = obj else {
        return (None, Vec::new());
    };

    // Per-code raw widths, walked once, up front — independent of which
    // text-decode branch below ends up winning (compute_string_width_ts
    // never looked at decoded text either). char_spacing/word_spacing are
    // NOT part of this array; see GlyphDecode's doc comment.
    let widths: Vec<RawGlyphWidth> = font_info
        .map(|fi| raw_glyph_widths(bytes, fi))
        .unwrap_or_default();
    let units_scale = font_info.map(|fi| fi.units_scale).unwrap_or(1.0);
    // Guarded, not just aligned-by-construction: `widths` is always indexed
    // the way `raw_glyph_widths`/`compute_string_width_ts` walk `bytes`
    // (CID pairs for an is_cid font, single bytes otherwise), but a few
    // fallback decode branches below are byte-indexed regardless of
    // is_cid (Differences map, the final single-byte catch-all) and can be
    // reached even for a CID font (e.g. one with no usable CMap at all but
    // an all-ASCII operand). A length mismatch there would silently
    // mis-pair units with the wrong widths — fall back to one opaque glyph
    // instead, exactly like the branches that are unaligned by design.
    let glyphs_for = |units: &[(u16, Option<String>)]| -> Vec<GlyphDecode> {
        if units.len() == widths.len() {
            glyphs_from_aligned_units(units, &widths, units_scale, font_size)
        } else {
            opaque_glyph(units_to_string(units), &widths, units_scale, font_size)
        }
    };
    let opaque_for =
        |text: String| -> Vec<GlyphDecode> { opaque_glyph(text, &widths, units_scale, font_size) };
    // Silence unused-parameter warnings on the placeholder-call path used by
    // extract_text_from_operand's wrapper (char_spacing/word_spacing are
    // intentionally not consumed here — see GlyphDecode's doc comment).
    let _ = (char_spacing, word_spacing);

    #[allow(clippy::type_complexity)]
    let mut decode_with_entry =
        |entry: &crate::tounicode::CMapEntry| -> Option<(String, Vec<GlyphDecode>)> {
            // For single-byte CMaps, merge CMap + Differences at the byte level:
            // try CMap first, then Differences, then Latin-1 fallback per byte.
            // This prevents partial CMap results from blocking the Differences path.
            if entry.primary.code_byte_length == 1 {
                let encoding_map = font_encodings.get(current_font);
                let mut units: Vec<(u16, Option<String>)> = Vec::with_capacity(bytes.len());
                for &b in bytes.iter() {
                    let code = b as u16;
                    let text: Option<String> = (|| {
                        // 1. Primary CMap
                        if let Some(s) = entry.primary.lookup(code) {
                            if !s.contains('\u{FFFD}') {
                                return Some(s);
                            }
                        }
                        // 2. Fallback CMap (embedded font cmap)
                        if let Some(fb) = entry.fallback.as_ref().and_then(|c| c.lookup(code)) {
                            if !fb.contains('\u{FFFD}') {
                                return Some(fb);
                            }
                        }
                        // 3. Differences mapped it? Use Differences result
                        if let Some(map) = encoding_map {
                            if let Some(&ch) = map.get(&b) {
                                return Some(ch.to_string());
                            }
                        }
                        // 4. Printable single-byte fallback
                        if b >= 0x20 {
                            return Some(
                                decode_single_byte_fallback_char(b, use_cp1252_fallback)
                                    .to_string(),
                            );
                        }
                        None
                    })();
                    units.push((code, text));
                }
                let decoded = units_to_string(&units);
                if !decoded.is_empty() {
                    return Some((decoded, glyphs_for(&units)));
                }
                return None;
            }

            // 2-byte CMap: use standard decode_cids path
            if bytes.len() % 2 == 1 {
                // Some PDFs emit 1-byte codes even for Type0 fonts; try per-byte lookup
                let lookups = entry.primary.lookup_bytes(bytes);
                let decoded: String = lookups
                    .iter()
                    .filter_map(|&(_b, ref cmap_result)| cmap_result.clone())
                    .collect();
                if !decoded.is_empty() {
                    // Byte-indexed (up to bytes.len() units), not aligned with
                    // `widths` (CID-indexed, floor(bytes.len()/2) units) —
                    // opaque, see GlyphDecode docs.
                    return Some((decoded.clone(), opaque_for(decoded)));
                }
            }
            let (primary_units, primary_failed) = entry.primary.decode_cids_glyphs(bytes);
            let decoded_primary = if primary_failed {
                String::new()
            } else {
                units_to_string(&primary_units)
            };
            if let Some(remapped) = entry.remapped.as_ref() {
                let (remap_units, remap_failed) = remapped.decode_cids_glyphs(bytes);
                let decoded_remap = if remap_failed {
                    String::new()
                } else {
                    units_to_string(&remap_units)
                };
                let fallback_pair = entry.fallback.as_ref().map(|c| c.decode_cids_glyphs(bytes));
                let decoded_fallback: Option<String> = fallback_pair.as_ref().map(|(u, failed)| {
                    if *failed {
                        String::new()
                    } else {
                        units_to_string(u)
                    }
                });

                if let Some(choice) = cmap_decisions
                    .get_choice(font_tounicode_refs.get(current_font).copied().unwrap_or(0))
                {
                    let (decoded, units) = match choice {
                        CMapChoice::Primary => (decoded_primary.clone(), &primary_units),
                        CMapChoice::Remapped => (decoded_remap.clone(), &remap_units),
                    };
                    if !decoded.is_empty() {
                        return Some((decoded, glyphs_for(units)));
                    }
                }

                let choice = cmap_decisions.consider(
                    font_tounicode_refs.get(current_font).copied().unwrap_or(0),
                    &decoded_primary,
                    &decoded_remap,
                    bytes.len(),
                );
                let (mut decoded, mut units) = match choice {
                    Some(CMapChoice::Primary) => (decoded_primary, primary_units),
                    Some(CMapChoice::Remapped) => (decoded_remap, remap_units),
                    // Mirrors choose_best_cmap_decode's exact branches (not
                    // called directly — it consumes/returns Strings and
                    // doesn't say which side won, which we also need here
                    // to pick the matching units vec).
                    None => {
                        if decoded_primary.is_empty() {
                            (decoded_remap, remap_units)
                        } else if decoded_remap.is_empty() {
                            (decoded_primary, primary_units)
                        } else if score_text(&decoded_remap) > score_text(&decoded_primary) + 3 {
                            (decoded_remap, remap_units)
                        } else {
                            (decoded_primary, primary_units)
                        }
                    }
                };
                if let Some(fb) = decoded_fallback {
                    let expected = bytes.len() / 2;
                    let decoded_len = decoded.chars().count();
                    let prefer_fallback = (!fb.is_empty() && decoded.is_empty())
                        || (!fb.is_empty() && expected > 0 && decoded_len * 2 < expected);
                    if prefer_fallback || score_text(&fb) > score_text(&decoded) + 3 {
                        decoded = fb;
                        units = fallback_pair.map(|(u, _)| u).unwrap_or_default();
                    }
                }
                if !decoded.is_empty() {
                    return Some((decoded, glyphs_for(&units)));
                }
            } else if !decoded_primary.is_empty() {
                let mut decoded = decoded_primary.clone();
                let mut units = primary_units;
                if let Some((fb_units, fb_failed)) =
                    entry.fallback.as_ref().map(|c| c.decode_cids_glyphs(bytes))
                {
                    let fb = if fb_failed {
                        String::new()
                    } else {
                        units_to_string(&fb_units)
                    };
                    let expected = bytes.len() / 2;
                    let decoded_len = decoded_primary.chars().count();
                    let prefer_fallback = (!fb.is_empty() && decoded_primary.is_empty())
                        || (!fb.is_empty() && expected > 0 && decoded_len * 2 < expected);
                    if prefer_fallback || score_text(&fb) > score_text(&decoded_primary) + 3 {
                        decoded = fb;
                        units = fb_units;
                    }
                }
                if !decoded.is_empty() {
                    return Some((decoded, glyphs_for(&units)));
                }
            }

            None
        };

    let (result_text, result_glyphs) = (|| -> (Option<String>, Vec<GlyphDecode>) {
        let mut has_cmap = false;
        if let Some(entry) = inline_cmaps.get(current_font) {
            has_cmap = true;
            if let Some((decoded, glyphs)) = decode_with_entry(entry) {
                return (Some(decoded), glyphs);
            }
        }

        // Look up CMap by ToUnicode object reference
        if let Some(&obj_num) = font_tounicode_refs.get(current_font) {
            if let Some(entry) = font_cmaps.get_by_obj(obj_num) {
                has_cmap = true;
                if let Some((decoded, glyphs)) = decode_with_entry(entry) {
                    return (Some(decoded), glyphs);
                }
            }
        }

        // CID fonts with a CMap that couldn't decode: the CID is genuinely
        // unmapped. Don't fall through to text-interpretation fallbacks
        // (Latin-1, UTF-16, etc.) which would misinterpret CID bytes as
        // character codes (e.g. CID 0x01A9 → Latin-1 "©").
        if is_type0_cid_font && bytes.iter().any(|&b| b > 0x7F) {
            // 2-byte CIDs (Identity-H) are by far the common case; for
            // an odd byte count we still emit at least one marker so
            // detection downstream fires.
            let cid_count = (bytes.len() / 2).max(1);
            let text = "\u{FFFD}".repeat(cid_count);
            // Aligned only when every placeholder has a real CID pair behind
            // it (widths has exactly cid_count entries); the `.max(1)` pad
            // for <2-byte input has no such pair — opaque in that case.
            if widths.len() == cid_count {
                let units: Vec<(u16, Option<String>)> = widths
                    .iter()
                    .map(|w| (w.code, Some("\u{FFFD}".to_string())))
                    .collect();
                return (Some(text), glyphs_for(&units));
            }
            return (Some(text.clone()), opaque_for(text));
        }

        // Try our custom encoding map from Differences arrays.
        // The Differences array overrides specific codes in a base encoding (typically
        // WinAnsiEncoding). We must combine Differences entries with the base encoding
        // rather than using filter_map which silently drops unmapped bytes.
        if let Some(encoding_map) = font_encodings.get(current_font) {
            let has_diff_match = bytes.iter().any(|b| encoding_map.contains_key(b));
            if has_diff_match {
                let units: Vec<(u16, Option<String>)> = bytes
                    .iter()
                    .map(|&b| {
                        let text = if let Some(&ch) = encoding_map.get(&b) {
                            Some(ch.to_string())
                        } else if b >= 0x20 {
                            // Base encoding fallback for printable bytes.
                            // Most PDFs with simple fonts use WinAnsi/PDFDocEncoding
                            // semantics, not ISO-8859-1 C1 controls.
                            Some(
                                decode_single_byte_fallback_char(b, use_cp1252_fallback)
                                    .to_string(),
                            )
                        } else {
                            None // Skip unmapped control characters
                        };
                        (b as u16, text)
                    })
                    .collect();
                let decoded = units_to_string(&units);
                if !decoded.is_empty() {
                    return (Some(decoded), glyphs_for(&units));
                }
            }
        }

        // Fallback: try UTF-16BE then Latin-1
        if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
            let utf16: Vec<u16> = bytes[2..]
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect();
            let text = String::from_utf16_lossy(&utf16);
            if text.contains('\u{FFFD}') {
                debug!(
                    "utf16 loss produced replacement for font={} bytes_len={}",
                    current_font,
                    bytes.len()
                );
            }
            return (Some(text.clone()), opaque_for(text));
        }

        // Heuristic UTF-16BE decode when bytes look like UTF-16 (even length, null-heavy)
        if bytes.len() >= 4 && bytes.len() % 2 == 0 {
            let nulls = bytes.iter().filter(|&&b| b == 0).count();
            if nulls * 4 > bytes.len() {
                let utf16: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                    .collect();
                let text = String::from_utf16_lossy(&utf16);
                if score_text(&text) > 0 {
                    return (Some(text.clone()), opaque_for(text));
                }
            }
        }

        // Check for UTF-8 encoded strings before single-byte encoding decoding.
        // Some PDFs incorrectly embed UTF-8 bytes in single-byte encoded fonts
        // (e.g. "José" as UTF-8 [C3 A9] instead of WinAnsi [E9]).
        if bytes.iter().any(|&b| b > 0x7F) {
            if let Ok(text) = std::str::from_utf8(bytes) {
                let text = text.to_string();
                return (Some(text.clone()), opaque_for(text));
            }
        }

        // Try to decode using cached font encoding from lopdf
        if let Some(encoding) = encoding_cache.get(current_font) {
            if let Ok(text) = Document::decode_text(encoding, bytes) {
                let text = normalize_cp1252_controls(text, use_cp1252_fallback);
                if text.contains('\u{FFFD}') {
                    debug!(
                        "decode_text produced replacement for font={} bytes_len={}",
                        current_font,
                        bytes.len()
                    );
                    if bytes.len() <= 8 {
                        let hex: String = bytes.iter().map(|b| format!("{:02X}", b)).collect();
                        debug!(
                            "decode_text replacement bytes font={} base={:?} hex={}",
                            current_font, base_font_name, hex
                        );
                    }
                    if bytes.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
                        let text: String = bytes.iter().map(|&b| b as char).collect();
                        return (Some(text.clone()), opaque_for(text));
                    }
                    if let Some(symbol_text) = decode_symbol_fallback(bytes, base_font_name) {
                        return (Some(symbol_text.clone()), opaque_for(symbol_text));
                    }
                    // For CID fonts (have ToUnicode CMap), the CID is
                    // genuinely unmapped — return None to avoid Latin-1
                    // fallback misinterpreting CID bytes as characters.
                    if has_cmap || font_tounicode_refs.contains_key(current_font) {
                        return (None, Vec::new());
                    }
                    // Non-CID fonts: fall through to other methods
                } else {
                    return (Some(text.clone()), opaque_for(text));
                }
            }
        }

        if let Some(symbol_text) = decode_symbol_fallback(bytes, base_font_name) {
            return (Some(symbol_text.clone()), opaque_for(symbol_text));
        }

        // Non-CID (Type1 / TrueType / Type3) fonts use single-byte
        // encodings. In practice the fallback should follow WinAnsi for
        // 0x80..=0x9F so bytes like 0x92 become smart punctuation instead
        // of C1 controls that look like CID mojibake.
        let units: Vec<(u16, Option<String>)> = bytes
            .iter()
            .map(|&b| {
                (
                    b as u16,
                    Some(decode_single_byte_fallback_char(b, use_cp1252_fallback).to_string()),
                )
            })
            .collect();
        let decoded = units_to_string(&units);
        (Some(decoded), glyphs_for(&units))
    })();

    let text = result_text.map(|text| {
        let text = clean_symbol_pua(text);
        let text = remap_texcm_math_symbols(text, base_font_name);
        normalize_cp1252_controls(text, use_cp1252_fallback)
    });
    (text, result_glyphs)
}

// No production caller since Phase 1 (content_stream.rs/xobjects.rs call
// decode_operand_glyphs directly, for the per-glyph breakdown) — kept for
// its own unit test coverage of the text-only decode path in isolation.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_text_from_operand(
    obj: &Object,
    current_font: &str,
    base_font_name: Option<&str>,
    font_cmaps: &FontCMaps,
    font_tounicode_refs: &std::collections::HashMap<String, u32>,
    inline_cmaps: &std::collections::HashMap<String, crate::tounicode::CMapEntry>,
    font_encodings: &PageFontEncodings,
    encoding_cache: &HashMap<String, Encoding<'_>>,
    cmap_decisions: &mut CMapDecisionCache,
    font_widths: &PageFontWidths,
) -> Option<String> {
    // Text decode never depends on font_size/char_spacing/word_spacing (only
    // GlyphDecode.width_ts does — see its doc comment), so any placeholder
    // values here are safe: this is byte-for-byte the same decode as before,
    // just reached through the unified function instead of a duplicate copy
    // of the same ~280 lines.
    decode_operand_glyphs(
        obj,
        current_font,
        base_font_name,
        font_cmaps,
        font_tounicode_refs,
        inline_cmaps,
        font_encodings,
        encoding_cache,
        cmap_decisions,
        font_widths,
        1.0,
        0.0,
        0.0,
    )
    .0
}

/// Fix a known producer bug in "TeXCMMathsSymbols" subset fonts (IntechOpen
/// and sibling academic pipelines): the Computer Modern symbol glyphs are
/// misnamed after Latin lookalikes (equal → /onequarter, plus → /thorn, …)
/// and the generated ToUnicode faithfully propagates the wrong names. The
/// remap applies only to text decoded from that font, keyed on the glyphs'
/// observed misnames.
fn remap_texcm_math_symbols(text: String, base_font_name: Option<&str>) -> String {
    let is_texcm = base_font_name.is_some_and(|n| {
        let n = n.rsplit_once('+').map_or(n, |(_, s)| s);
        n.eq_ignore_ascii_case("TeXCMMathsSymbols")
    });
    if !is_texcm {
        return text;
    }
    text.chars()
        .map(|c| match c {
            '¼' => '=',
            '½' => '-',
            'þ' => '+',
            'ð' => '(',
            'Þ' => ')',
            _ => c,
        })
        .collect()
}

// decode_operand_glyphs's final catch-all now builds the equivalent
// per-byte units inline (to also produce GlyphDecode output), so this is no
// longer called directly; kept as the single-byte-fallback primitive name
// in case a future caller wants just the string.
#[allow(dead_code)]
fn decode_single_byte_fallback(bytes: &[u8], use_cp1252_fallback: bool) -> String {
    bytes
        .iter()
        .map(|&b| decode_single_byte_fallback_char(b, use_cp1252_fallback))
        .collect()
}

fn decode_single_byte_fallback_char(byte: u8, use_cp1252_fallback: bool) -> char {
    if !use_cp1252_fallback {
        return byte as char;
    }

    match byte {
        0x80 => '\u{20AC}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8E => '\u{017D}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        _ => byte as char,
    }
}

fn normalize_cp1252_controls(text: String, use_cp1252_fallback: bool) -> String {
    if !use_cp1252_fallback {
        return text;
    }
    if !text
        .chars()
        .any(|ch| ('\u{0080}'..='\u{009F}').contains(&ch))
    {
        return text;
    }

    text.chars()
        .map(|ch| {
            if ('\u{0080}'..='\u{009F}').contains(&ch) {
                decode_single_byte_fallback_char(ch as u8, true)
            } else {
                ch
            }
        })
        .collect()
}

fn should_use_cp1252_single_byte_fallback(
    base_font_name: Option<&str>,
    is_type0_cid_font: bool,
) -> bool {
    if is_type0_cid_font {
        return false;
    }

    let Some(base_font_name) = base_font_name else {
        return true;
    };
    let font_name = base_font_name
        .rsplit_once('+')
        .map_or(base_font_name, |(_, stripped)| stripped)
        .to_ascii_lowercase();

    // TeX/Computer Modern and math/symbol fonts often place ligatures or
    // symbols in the C1 byte range. Treating those bytes as Windows-1252 makes
    // words like "deficiente" become "de…ciente" and "fluid" become "‡uid".
    let non_cp1252_prefixes = [
        "cmr", "cmb", "cmmi", "cmsy", "cmex", "cmtt", "cmss", "cmti", "ecrm", "ecbx", "ecti",
        "tcrm", "tctt", "msam", "msbm", "ttdc",
    ];
    if non_cp1252_prefixes
        .iter()
        .any(|prefix| font_name.starts_with(prefix))
    {
        return false;
    }

    let non_cp1252_names = ["math", "symbol", "dingbat", "emoji"];
    !non_cp1252_names.iter().any(|name| font_name.contains(name))
}

/// Replace PUA characters in the F000-F0FF range with standard Unicode equivalents.
/// These come from Symbol/Wingdings fonts whose ToUnicode CMaps map to PUA.
fn clean_symbol_pua(text: String) -> String {
    if !text.chars().any(|c| ('\u{F000}'..='\u{F0FF}').contains(&c)) {
        return text;
    }
    text.chars()
        .map(|c| {
            let code = c as u32;
            if !(0xF000..=0xF0FF).contains(&code) {
                return c;
            }
            let low = code - 0xF000;
            match low {
                // Common bullets
                0xA1 | 0xA7 | 0xB7 => '\u{2022}',
                // Checkmark
                0xFC => '\u{2713}',
                // Printable ASCII range and Latin-1 above: strip F000 offset
                0x20..=0xFF => char::from_u32(low).unwrap_or(c),
                _ => c,
            }
        })
        .collect()
}

fn decode_symbol_fallback(bytes: &[u8], base_font_name: Option<&str>) -> Option<String> {
    let name = base_font_name?.to_ascii_lowercase();
    if !name.contains("symbol") && !name.contains("wingdings") && !name.contains("zapfdingbats") {
        return None;
    }
    let mut out = String::new();
    for &b in bytes {
        if b < 0x20 {
            continue;
        }
        if let Some(ch) = char::from_u32(0xF000 + b as u32) {
            out.push(ch);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// No longer called directly (decode_operand_glyphs inlines the same
// branches so it can also track which side — primary or remapped — won,
// needed to pick the matching per-glyph units vec); kept for its unit test
// coverage of the tie-break logic in isolation.
#[allow(dead_code)]
fn choose_best_cmap_decode(primary: String, remapped: String) -> String {
    if primary.is_empty() {
        return remapped;
    }
    if remapped.is_empty() {
        return primary;
    }
    let score_primary = score_text(&primary);
    let score_remap = score_text(&remapped);
    if score_remap > score_primary + 3 {
        remapped
    } else {
        primary
    }
}

fn score_text(text: &str) -> i32 {
    const COMMON_WORDS: [&str; 22] = [
        "the", "and", "of", "to", "in", "a", "is", "that", "for", "with", "on", "as", "by", "from",
        "this", "be", "are", "at", "or", "not", "it", "our",
    ];

    let mut letters = 0i32;
    let mut spaces = 0i32;
    let mut digits = 0i32;
    let mut other = 0i32;
    let mut word_hits = 0i32;

    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphabetic() {
            letters += 1;
            current.push(ch.to_ascii_lowercase());
        } else {
            if !current.is_empty() {
                if COMMON_WORDS.iter().any(|w| *w == current) {
                    word_hits += 1;
                }
                current.clear();
            }
            if ch == ' ' {
                spaces += 1;
            } else if ch.is_ascii_digit() {
                digits += 1;
            } else if ch.is_control() || ch == '\u{FFFD}' {
                other += 3;
            } else if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
                || ('\u{3040}'..='\u{309F}').contains(&ch)
                || ('\u{30A0}'..='\u{30FF}').contains(&ch)
                || ('\u{3400}'..='\u{4DBF}').contains(&ch)
                || ('\u{F900}'..='\u{FAFF}').contains(&ch)
            {
                letters += 1; // CJK ideographs / kana count as valid text
            } else {
                other += 1;
            }
        }
    }
    if !current.is_empty() && COMMON_WORDS.iter().any(|w| *w == current) {
        word_hits += 1;
    }

    let mut score = word_hits * 10 + letters + spaces * 2 + digits - other * 2;
    if letters > 15 && word_hits == 0 {
        score -= 15;
    }
    score
}

#[cfg(test)]
mod tests {

    #[test]
    fn item_font_name_prefers_family_over_resource_tag() {
        use super::item_font_name;
        assert_eq!(item_font_name("F2", "ABCDEF+CMMI10"), "ABCDEF+CMMI10");
        assert_eq!(item_font_name("T22", "Times-Roman"), "Times-Roman");
        // Distiller CID-convention resources keep the resource name:
        // is_cid_font keys on the C2_/C0_ prefix for micro-gap joining.
        assert_eq!(item_font_name("C2_0", "ABCDEE+SimSun"), "C2_0");
        assert_eq!(item_font_name("C0_1", "ABCDEE+MSMincho"), "C0_1");
    }

    #[test]
    fn type3_scale_resolves_indirect_matrix_and_bbox_numbers() {
        use lopdf::{dictionary, Document, Object};
        // FontMatrix/FontBBox elements may be indirect references per PDF
        // syntax; the scale must use their resolved values, not zero.
        let mut doc = Document::with_version("1.4");
        let matrix_d = doc.add_object(Object::Real(-1.0));
        let bbox_top = doc.add_object(Object::Integer(3));
        let font_dict = dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "FontMatrix" => vec![
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(0),
                Object::Reference(matrix_d),
                Object::Integer(0),
                Object::Integer(0),
            ],
            "FontBBox" => vec![
                Object::Integer(1),
                Object::Integer(-156),
                Object::Integer(37),
                Object::Reference(bbox_top),
            ],
        };
        let mut fonts = std::collections::BTreeMap::new();
        fonts.insert(b"T2".to_vec(), &font_dict);
        let scales = super::build_type3_scales(&doc, &fonts);
        let scale = scales.get("T2").copied().unwrap_or(1.0);
        // bbox height 159 x |matrix_y| 1.0
        assert!(
            (scale - 159.0).abs() < 0.5,
            "scale should use resolved indirect values, got {scale}"
        );
    }

    /// Build a one-font Type3 document and return its computed scale, if any.
    #[cfg(test)]
    fn type3_scale_for(matrix_y: f32, bbox_lo: i64, bbox_hi: i64) -> Option<f32> {
        use lopdf::{dictionary, Document, Object};
        let doc = Document::with_version("1.4");
        let font_dict = dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "FontMatrix" => vec![
                Object::Real(matrix_y), Object::Integer(0), Object::Integer(0),
                Object::Real(matrix_y), Object::Integer(0), Object::Integer(0),
            ],
            "FontBBox" => vec![
                Object::Integer(0), Object::Integer(bbox_lo),
                Object::Integer(600), Object::Integer(bbox_hi),
            ],
        };
        let mut fonts = std::collections::BTreeMap::new();
        fonts.insert(b"T9".to_vec(), &font_dict);
        super::build_type3_scales(&doc, &fonts).get("T9").copied()
    }

    #[test]
    fn type3_scale_skips_self_consistent_fonts() {
        // Conventional 1/1000 matrix with a descender..ascender bbox of 700
        // units: scale 0.7. The Tf operand is already the rendered size, so
        // renormalizing would report every size at 0.7x.
        assert_eq!(type3_scale_for(0.001, -200, 500), None);
        // Tall-accent bbox slightly over the em (1100 units, scale 1.1).
        assert_eq!(type3_scale_for(0.001, -100, 1000), None);
    }

    #[test]
    fn type3_scale_applies_to_inconsistent_fonts_at_any_matrix_scale() {
        // Non-standard but valid matrix (0.005) with a full-em bbox:
        // scale 5.0, so the declared size is off by 5x and must be fixed.
        let s = type3_scale_for(0.005, 0, 1000).expect("0.005 matrix should rescale");
        assert!((s - 5.0).abs() < 0.01, "got {s}");
        // dvips/PK bitmap pattern: unit matrix, glyphs spanning ~159 units.
        let s = type3_scale_for(1.0, -156, 3).expect("PK pattern should rescale");
        assert!((s - 159.0).abs() < 0.5, "got {s}");
    }

    #[test]
    fn type3_scale_ignores_degenerate_bbox() {
        // [0 0 0 0] is legal and carries no size information.
        assert_eq!(type3_scale_for(0.001, 0, 0), None);
    }

    #[test]
    fn texcm_math_symbols_remap() {
        assert_eq!(
            super::remap_texcm_math_symbols("S ¼ kB þ 1".into(), Some("EEKVNO+TeXCMMathsSymbols")),
            "S = kB + 1"
        );
        // Other fonts keep their genuine fractions/thorns.
        assert_eq!(
            super::remap_texcm_math_symbols("¼ cup þorn".into(), Some("Times-Roman")),
            "¼ cup þorn"
        );
        assert_eq!(super::remap_texcm_math_symbols("¼".into(), None), "¼");
    }

    use super::*;
    use lopdf::dictionary;

    fn make_font_info(widths: &[(u16, u16)], default_width: u16, is_cid: bool) -> FontWidthInfo {
        FontWidthInfo {
            widths: widths.iter().copied().collect(),
            default_width,
            space_width: widths
                .iter()
                .find(|(k, _)| *k == 32)
                .map(|(_, v)| *v)
                .unwrap_or(default_width),
            is_cid,
            units_scale: 0.001,
            wmode: 0,
        }
    }

    fn doc_with_descriptor(descriptor: lopdf::Dictionary) -> (Document, lopdf::Dictionary) {
        let mut doc = Document::with_version("1.4");
        let desc_id = doc.add_object(descriptor);
        let font_dict = dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => "Tc1",
            "FontDescriptor" => desc_id,
        };
        (doc, font_dict)
    }

    #[test]
    fn descriptor_italic_angle_sets_italic() {
        // Subset font with an opaque BaseFont name ("Tc1") — the name
        // heuristic sees nothing, the descriptor carries the truth.
        let (doc, font_dict) = doc_with_descriptor(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Tc1",
            "ItalicAngle" => -12,
            "Flags" => 32,
        });
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut FontStyleCache::new()),
            (true, false)
        );
    }

    #[test]
    fn descriptor_italic_flag_bit_sets_italic() {
        let (doc, font_dict) = doc_with_descriptor(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Tc1",
            "ItalicAngle" => 0,
            "Flags" => 64, // bit 7: Italic
        });
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut FontStyleCache::new()),
            (true, false)
        );
    }

    #[test]
    fn descriptor_force_bold_flag_sets_bold() {
        let (doc, font_dict) = doc_with_descriptor(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Tc1",
            "ItalicAngle" => 0,
            "Flags" => 1 << 18, // ForceBold
        });
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut FontStyleCache::new()),
            (false, true)
        );
    }

    #[test]
    fn tiny_italic_angle_is_not_italic() {
        // A token 1-degree slant is optical correction, not italic.
        let (doc, font_dict) = doc_with_descriptor(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Tc1",
            "ItalicAngle" => lopdf::Object::Real(-1.0),
            "Flags" => 32,
        });
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut FontStyleCache::new()),
            (false, false)
        );
    }

    #[test]
    fn missing_descriptor_yields_no_flags() {
        let doc = Document::with_version("1.4");
        let font_dict = dictionary! { "Type" => "Font", "BaseFont" => "Tc1" };
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut FontStyleCache::new()),
            (false, false)
        );
    }

    #[test]
    fn type0_descendant_descriptor_is_resolved() {
        let mut doc = Document::with_version("1.4");
        let desc_id = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "ABCDEF+F1",
            "ItalicAngle" => -15,
        });
        let cid_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "FontDescriptor" => desc_id,
        });
        let font_dict = dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "ABCDEF+F1",
            "DescendantFonts" => vec![lopdf::Object::Reference(cid_id)],
        };
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut FontStyleCache::new()),
            (true, false)
        );
    }

    /// Bare CFF: header + Name INDEX only — enough for `cff_font_name`.
    fn bare_cff_with_name(name: &str) -> Vec<u8> {
        let mut data = vec![1, 0, 4, 1]; // major, minor, hdrSize, offSize
        data.extend_from_slice(&1u16.to_be_bytes()); // Name INDEX count
        data.push(1); // offSize
        data.push(1); // offset of first name
        data.push(1 + name.len() as u8); // offset past last name
        data.extend_from_slice(name.as_bytes());
        data
    }

    #[test]
    fn embedded_font_style_is_cached_by_font_file_object() {
        use lopdf::{Object, Stream};

        let mut doc = Document::with_version("1.4");
        let ff_id = doc.add_object(Object::Stream(Stream::new(
            dictionary! {},
            bare_cff_with_name("ABCDEF+Test-BoldItalic"),
        )));
        let desc_id = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "ABCDEF+Test-BoldItalic",
            "ItalicAngle" => 0,
            "Flags" => 32,
            "FontFile3" => ff_id,
        });
        let font_dict = dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Tc1",
            "FontDescriptor" => desc_id,
        };

        let mut cache = FontStyleCache::new();
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut cache),
            (true, true)
        );
        assert_eq!(cache.by_font_file.len(), 1);

        // Replace the font program with garbage: a repeat call must serve
        // the memo instead of re-reading the stream — repeated per-page
        // decompression is exactly what the cache exists to avoid.
        doc.objects.insert(
            ff_id,
            Object::Stream(Stream::new(dictionary! {}, vec![0u8; 4])),
        );
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut cache),
            (true, true)
        );
        // A cold cache parses the (now garbage) stream, proving the warm
        // call above answered from the memo.
        assert_eq!(
            descriptor_style_flags(&doc, &font_dict, &mut FontStyleCache::new()),
            (false, false)
        );
    }

    #[test]
    fn compute_string_width_ts_no_tc_tw() {
        // Without Tc/Tw (both 0), width = glyph widths only
        let fi = make_font_info(&[(72, 500), (101, 400), (108, 300)], 600, false);
        let bytes = b"Hello"; // H=500, e=400, l=300, l=300, o=600(default)
        let w = compute_string_width_ts(bytes, &fi, 10.0, 0.0, 0.0);
        // (500+400+300+300+600) * 0.001 * 10 = 21.0
        assert!((w - 21.0).abs() < 0.01);
    }

    #[test]
    fn compute_string_width_ts_with_positive_tc() {
        // Positive Tc adds char_spacing per character
        let fi = make_font_info(&[], 500, false);
        let bytes = b"ab"; // 2 chars, each 500 default
        let w = compute_string_width_ts(bytes, &fi, 10.0, 0.5, 0.0);
        // glyph: (500+500)*0.001*10 = 10.0, Tc: 2*0.5 = 1.0, total = 11.0
        assert!((w - 11.0).abs() < 0.01);
    }

    #[test]
    fn compute_string_width_ts_with_negative_tc() {
        // Negative Tc (tight tracking) reduces width
        let fi = make_font_info(&[], 500, false);
        let bytes = b"ab";
        let w = compute_string_width_ts(bytes, &fi, 10.0, -0.3, 0.0);
        // glyph: 10.0, Tc: 2*(-0.3) = -0.6, total = 9.4
        assert!((w - 9.4).abs() < 0.01);
    }

    #[test]
    fn compute_string_width_ts_with_tw() {
        // Tw applies only to space characters (byte 0x20)
        let fi = make_font_info(&[(32, 250)], 500, false);
        let bytes = b"a b"; // 'a'=500, ' '=250, 'b'=500
        let w = compute_string_width_ts(bytes, &fi, 10.0, 0.0, 0.8);
        // glyph: (500+250+500)*0.001*10 = 12.5, Tw: 1*0.8 = 0.8, total = 13.3
        assert!((w - 13.3).abs() < 0.01);
    }

    #[test]
    fn compute_string_width_ts_with_tc_and_tw() {
        // Both Tc and Tw
        let fi = make_font_info(&[(32, 250)], 500, false);
        let bytes = b"a b"; // 3 chars, 1 space
        let w = compute_string_width_ts(bytes, &fi, 10.0, 0.1, 0.5);
        // glyph: 12.5, Tc: 3*0.1 = 0.3, Tw: 1*0.5 = 0.5, total = 13.3
        assert!((w - 13.3).abs() < 0.01);
    }

    #[test]
    fn compute_string_width_ts_cid_font() {
        // CID font: 2-byte codes, space is CID 32
        let fi = make_font_info(&[(65, 500), (32, 250)], 600, true);
        // "A " in CID: [0,65, 0,32]
        let bytes = &[0u8, 65, 0, 32];
        let w = compute_string_width_ts(bytes, &fi, 12.0, 0.2, 0.3);
        // glyph: (500+250)*0.001*12 = 9.0, Tc: 2*0.2 = 0.4, Tw: 1*0.3 = 0.3
        assert!((w - 9.7).abs() < 0.01);
    }

    #[test]
    fn ligature_cid_produces_first_real_glyph_plus_zero_width_filler() {
        // Real Lam-Alef ligature CID from the Aspose-produced Etimad tender
        // corpus (نموذج_كراسة_عام.pdf, font /F1, BCDEEE+DINNextLTArabic-
        // Regular, found tracing the Instance 2 investigation): CID 0x01D9
        // -> "لا" (U+0644 LAM, U+0627 ALEF), font-unit width 680.
        //
        // Confirms decode_operand_glyphs's ligature split (Phase 1 design
        // point 2): one real glyph carrying the CID's own width for the
        // first character, one zero-width filler (cid=None) for the second
        // — mirroring MuPDF's fz_show_glyph_aux filler-glyph convention for
        // exactly this case — while the aggregate width from
        // compute_string_width_ts (untouched) stays exactly what it was
        // before Phase 1.
        let cmap_content = br#"
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
1 beginbfrange
<01D9> <01D9> [<06440627>]
endbfrange
"#;
        let cmap = crate::tounicode::ToUnicodeCMap::parse(cmap_content).unwrap();
        assert_eq!(cmap.code_byte_length, 2);
        assert_eq!(cmap.lookup(0x01D9), Some("لا".to_string()));

        let entry = crate::tounicode::CMapEntry {
            primary: cmap,
            remapped: None,
            fallback: None,
        };
        let mut inline_cmaps = HashMap::new();
        inline_cmaps.insert("F1".to_string(), entry);

        let mut font_widths: PageFontWidths = HashMap::new();
        font_widths.insert(
            "F1".to_string(),
            make_font_info(&[(0x01D9, 680)], 1000, true),
        );

        let bytes = vec![0x01u8, 0xD9]; // CID 0x01D9, big-endian
        let obj = Object::String(bytes.clone(), lopdf::StringFormat::Hexadecimal);

        let font_cmaps = FontCMaps::default();
        let font_tounicode_refs: HashMap<String, u32> = HashMap::new();
        let font_encodings: PageFontEncodings = HashMap::new();
        let encoding_cache: HashMap<String, Encoding<'_>> = HashMap::new();
        let mut decisions = CMapDecisionCache::new();

        let (text, glyphs) = decode_operand_glyphs(
            &obj,
            "F1",
            None,
            &font_cmaps,
            &font_tounicode_refs,
            &inline_cmaps,
            &font_encodings,
            &encoding_cache,
            &mut decisions,
            &font_widths,
            12.0,
            0.0,
            0.0,
        );

        assert_eq!(text.as_deref(), Some("لا"));
        assert_eq!(
            glyphs.len(),
            2,
            "one real + one filler glyph, not one per string"
        );

        assert_eq!(glyphs[0].text, "ل");
        assert_eq!(glyphs[0].cid, Some(0x01D9));
        let expected_width = 680.0 * 0.001 * 12.0; // raw_width * units_scale * font_size
        assert!(
            (glyphs[0].width_ts - expected_width).abs() < 0.001,
            "first glyph should carry the CID's real width, got {}",
            glyphs[0].width_ts
        );
        // Phase 2: the real glyph represents exactly the 1 underlying CID
        // (Tc-chargeable), and it isn't a space (CID != 32) so no Tw.
        assert_eq!(glyphs[0].code_count, 1);
        assert_eq!(glyphs[0].space_count, 0);
        assert_eq!(
            glyphs[0].pen, None,
            "pen position is the caller's job, unset here"
        );

        assert_eq!(glyphs[1].text, "ا");
        assert_eq!(glyphs[1].cid, None, "filler glyph has no CID of its own");
        assert_eq!(glyphs[1].width_ts, 0.0, "filler glyph must be zero-width");
        // Phase 2: the filler doesn't independently consume a code — the
        // real glyph above already charged Tc/Tw for the whole CID.
        assert_eq!(glyphs[1].code_count, 0);
        assert_eq!(glyphs[1].space_count, 0);

        // The aggregate that actually drives positioning is untouched by
        // Phase 1 — same value the pre-refactor two-function code produced.
        let fi = font_widths.get("F1").unwrap();
        let agg = compute_string_width_ts(&bytes, fi, 12.0, 0.0, 0.0);
        assert!((agg - expected_width).abs() < 0.001);

        // Phase 2: glyph_advance_ts, summed over both glyphs, reproduces
        // that same aggregate — the property pen-position tracking relies
        // on (see glyph_advance_ts's doc comment).
        let summed: f32 = glyphs.iter().map(|g| glyph_advance_ts(g, 0.0, 0.0)).sum();
        assert!((summed - agg).abs() < 0.001);
    }

    #[test]
    fn pen_track_glyphs_simple_ltr_advances_left_to_right() {
        // Phase 2, verification requirement 3 (simple case): a plain LTR
        // run should have each glyph's pen.x strictly increasing by that
        // glyph's own advance, pen.y unchanged — no CMap/font machinery
        // needed, this only exercises pen_track_glyphs itself.
        let fi = make_font_info(
            &[(b'A' as u16, 600), (b'B' as u16, 500), (b'C' as u16, 700)],
            500,
            false,
        );
        let bytes = b"ABC";
        let (units, failed) = {
            // Simple (non-CID) font: one "unit" per byte, decoded text is
            // irrelevant here — build GlyphDecode directly via the same
            // real code path decode_operand_glyphs uses for simple fonts.
            let cmap_content = br#"
1 begincodespacerange
<00> <FF>
endcodespacerange
3 beginbfchar
<41> <0041>
<42> <0042>
<43> <0043>
endbfchar
"#;
            let cmap = crate::tounicode::ToUnicodeCMap::parse(cmap_content).unwrap();
            assert_eq!(cmap.code_byte_length, 1);
            cmap.decode_cids_glyphs(bytes)
        };
        assert!(!failed);
        let widths: Vec<RawGlyphWidth> = raw_glyph_widths(bytes, &fi);
        let glyphs_vec = glyphs_from_aligned_units(&units, &widths, fi.units_scale, 10.0);
        let mut glyphs = glyphs_vec;

        let font_size = 10.0;
        let _ = font_size; // already baked into glyphs_from_aligned_units above
        let start_tm = [1.0, 0.0, 0.0, 1.0, 100.0, 700.0];
        let ctm = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let end_tm = crate::extractor::pen_track_glyphs(&mut glyphs, &start_tm, &ctm, 0.0, 0.0);

        assert_eq!(glyphs.len(), 3);
        // A=600 units, B=500, C=700, units_scale=0.001, font_size=10 =>
        // advances 6.0, 5.0, 7.0
        let a_pen = glyphs[0].pen.expect("pen must be set");
        let b_pen = glyphs[1].pen.expect("pen must be set");
        let c_pen = glyphs[2].pen.expect("pen must be set");
        assert!(
            (a_pen.0 - 100.0).abs() < 0.001,
            "first glyph starts at Tm's x"
        );
        assert!(
            (a_pen.1 - 700.0).abs() < 0.001,
            "y unchanged for horizontal text"
        );
        assert!(
            (b_pen.0 - 106.0).abs() < 0.001,
            "B starts after A's 6.0 advance"
        );
        assert!(
            (c_pen.0 - 111.0).abs() < 0.001,
            "C starts after A+B's 6.0+5.0 advance"
        );
        // Strictly increasing x, unchanged y — the LTR property this test
        // exists to check.
        assert!(a_pen.0 < b_pen.0 && b_pen.0 < c_pen.0);
        assert_eq!(a_pen.1, b_pen.1);
        assert_eq!(b_pen.1, c_pen.1);
        assert!(
            (end_tm[4] - 118.0).abs() < 0.001,
            "ending matrix past C's 7.0 advance"
        );
    }

    #[test]
    fn pen_track_glyphs_matches_real_fixture_dump_ops_trace() {
        // Phase 2, verification requirement 3 (real RTL case): the exact
        // CID sequence `<011001d9> -5 <0164> 4 <0113>` traced via dump_ops
        // from a real production fixture
        // (نموذج_كراسة_عام.pdf, page 6, font /F1 BCDEEE+DINNextLTArabic-
        // Regular, Tf 11.04, Tm [1 0 0 1 477.7 553.27]) — the same run
        // containing the Lam-Alef ligature CID (0x01D9 -> "لا") Phase 1's
        // own ligature test uses, here with its real dump_ops-observed
        // neighbors and kerning numbers. Font-unit widths below (CID ->
        // width) are read directly from that font's own /W array in the
        // real PDF: 0x0110->281, 0x01D9->680, 0x0164->265, 0x0113->224 —
        // not fabricated. Expected positions are hand-computed from those
        // same real widths/kerning/Tm using the PDF spec's own tx formula
        // (independently of pen_track_glyphs's implementation, just using
        // the same inputs) — see the arithmetic in each assertion's
        // comment.
        let cmap_content = br#"
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
3 beginbfchar
<0110> <0645>
<0164> <062F>
<0113> <0020>
endbfchar
1 beginbfrange
<01D9> <01D9> [<06440627>]
endbfrange
"#;
        let cmap = crate::tounicode::ToUnicodeCMap::parse(cmap_content).unwrap();
        let entry = crate::tounicode::CMapEntry {
            primary: cmap,
            remapped: None,
            fallback: None,
        };
        let mut inline_cmaps = HashMap::new();
        inline_cmaps.insert("F1".to_string(), entry);

        let mut font_widths: PageFontWidths = HashMap::new();
        font_widths.insert(
            "F1".to_string(),
            make_font_info(
                &[(0x0110, 281), (0x01D9, 680), (0x0164, 265), (0x0113, 224)],
                1000,
                true,
            ),
        );

        let font_cmaps = FontCMaps::default();
        let font_tounicode_refs: HashMap<String, u32> = HashMap::new();
        let font_encodings: PageFontEncodings = HashMap::new();
        let encoding_cache: HashMap<String, Encoding<'_>> = HashMap::new();
        let mut decisions = CMapDecisionCache::new();

        // The array's 2 string elements, decoded independently exactly as
        // content_stream.rs's TJ handler does, with the SAME real kerning
        // number (-5, in thousandths of an em) between them.
        let font_size = 11.04f32;
        let obj1 = Object::String(
            vec![0x01, 0x10, 0x01, 0xD9],
            lopdf::StringFormat::Hexadecimal,
        );
        let (_, mut glyphs1) = decode_operand_glyphs(
            &obj1,
            "F1",
            None,
            &font_cmaps,
            &font_tounicode_refs,
            &inline_cmaps,
            &font_encodings,
            &encoding_cache,
            &mut decisions,
            &font_widths,
            font_size,
            0.0,
            0.0,
        );
        assert_eq!(
            glyphs1.len(),
            3,
            "CID 0x0110 (1 glyph) + CID 0x01D9 (real+filler)"
        );

        let obj2 = Object::String(vec![0x01, 0x64], lopdf::StringFormat::Hexadecimal);
        let (_, mut glyphs2) = decode_operand_glyphs(
            &obj2,
            "F1",
            None,
            &font_cmaps,
            &font_tounicode_refs,
            &inline_cmaps,
            &font_encodings,
            &encoding_cache,
            &mut decisions,
            &font_widths,
            font_size,
            0.0,
            0.0,
        );

        let obj3 = Object::String(vec![0x01, 0x13], lopdf::StringFormat::Hexadecimal);
        let (_, mut glyphs3) = decode_operand_glyphs(
            &obj3,
            "F1",
            None,
            &font_cmaps,
            &font_tounicode_refs,
            &inline_cmaps,
            &font_encodings,
            &encoding_cache,
            &mut decisions,
            &font_widths,
            font_size,
            0.0,
            0.0,
        );

        let start_tm = [1.0f32, 0.0, 0.0, 1.0, 477.7, 553.27];
        let ctm = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];

        // Element 1: <011001d9>
        let tm_after_1 =
            crate::extractor::pen_track_glyphs(&mut glyphs1, &start_tm, &ctm, 0.0, 0.0);
        // Kerning -5 (thousandths of em): displacement = -(-5)/1000*11.04 = +0.0552
        let mut tm_after_kern1 = tm_after_1;
        tm_after_kern1[4] += 5.0 / 1000.0 * font_size;
        // Element 2: <0164>
        let tm_after_2 =
            crate::extractor::pen_track_glyphs(&mut glyphs2, &tm_after_kern1, &ctm, 0.0, 0.0);
        // Kerning 4: displacement = -4/1000*11.04 = -0.04416
        let mut tm_after_kern2 = tm_after_2;
        tm_after_kern2[4] -= 4.0 / 1000.0 * font_size;
        // Element 3: <0113>
        crate::extractor::pen_track_glyphs(&mut glyphs3, &tm_after_kern2, &ctm, 0.0, 0.0);

        // CID 0x0110 (width 281): pen == Tm's own start.
        let pen_0110 = glyphs1[0].pen.expect("pen must be set");
        assert!((pen_0110.0 - 477.7).abs() < 0.01, "got {}", pen_0110.0);
        assert!((pen_0110.1 - 553.27).abs() < 0.01);

        // CID 0x01D9 real glyph "ل": 477.7 + 281*0.001*11.04 = 477.7 + 3.10224 = 480.80224
        let pen_01d9_real = glyphs1[1].pen.expect("pen must be set");
        assert!(
            (pen_01d9_real.0 - 480.80224).abs() < 0.01,
            "got {}",
            pen_01d9_real.0
        );

        // Filler "ا": same position as the real glyph before it (0 advance).
        let pen_01d9_filler = glyphs1[2].pen.expect("pen must be set");
        assert!(
            (pen_01d9_filler.0 - pen_01d9_real.0).abs() < 0.0001,
            "filler must not move the pen"
        );

        // CID 0x0164 (width 265), after the -5 kerning bump:
        // 480.80224 + 680*0.001*11.04 + 5/1000*11.04
        //   = 480.80224 + 7.5072 + 0.0552 = 488.36464
        let pen_0164 = glyphs2[0].pen.expect("pen must be set");
        assert!((pen_0164.0 - 488.36464).abs() < 0.01, "got {}", pen_0164.0);

        // CID 0x0113 (width 224), after the +4 kerning bump (displacement -0.04416):
        // 488.36464 + 265*0.001*11.04 - 4/1000*11.04 = 488.36464 + 2.9256 - 0.04416 = 491.24608
        let pen_0113 = glyphs3[0].pen.expect("pen must be set");
        assert!((pen_0113.0 - 491.24608).abs() < 0.01, "got {}", pen_0113.0);

        // y never changes across this whole horizontal run.
        for pen in [pen_0110, pen_01d9_real, pen_01d9_filler, pen_0164, pen_0113] {
            assert!((pen.1 - 553.27).abs() < 0.01);
        }
    }

    #[test]
    fn compute_string_width_ts_large_tc() {
        // Large Tc (character-spreading) is applied in full
        let fi = make_font_info(&[], 500, false);
        let bytes = b"abc"; // 3 chars
        let w = compute_string_width_ts(bytes, &fi, 10.0, 5.0, 0.0);
        // glyph: (500*3)*0.001*10 = 15.0, Tc: 3*5.0 = 15.0, total = 30.0
        assert!((w - 30.0).abs() < 0.01);
    }

    #[test]
    fn score_text_cjk() {
        // Correct Japanese text should score well
        let japanese = "2026年9月期 1Q 業績報告";
        // Garbled output (random CJK from wrong remap)
        let garbled = "\u{FFFD}\u{FFFD}\u{FFFD}";

        let s_jp = score_text(japanese);
        let s_garbled = score_text(garbled);
        assert!(
            s_jp > s_garbled,
            "Japanese text ({s_jp}) should score higher than garbled ({s_garbled})"
        );
    }

    #[test]
    fn score_text_cjk_vs_ascii_garbage() {
        // Real CJK text
        let cjk = "株式会社の業績についてご報告いたします";
        // Ascii garbage of similar length
        let garbage = "}{|~`^@#$%&*()!<>[];:',./";

        let s_cjk = score_text(cjk);
        let s_garbage = score_text(garbage);
        assert!(
            s_cjk > s_garbage,
            "CJK text ({s_cjk}) should score higher than garbage ({s_garbage})"
        );
    }

    #[test]
    fn score_text_english_still_works() {
        let good = "the quick brown fox and the lazy dog";
        let bad = "###!!!@@@$$$";
        assert!(score_text(good) > score_text(bad));
    }

    fn doc_with_private_differences() -> (Document, lopdf::ObjectId) {
        let mut doc = Document::with_version("1.7");
        let encoding_id = doc.add_object(dictionary! {
            "Differences" => Object::Array(vec![
                Object::Integer(0x88),
                Object::Name(b"g431".to_vec()),
                Object::Name(b"fi".to_vec()),
                Object::Integer(0xAD),
                Object::Name(b"fl".to_vec()),
            ]),
        });

        (doc, encoding_id)
    }

    #[test]
    fn aptos_private_g431_maps_to_ff_ligature() {
        let (doc, encoding_id) = doc_with_private_differences();
        let font_dict = dictionary! {
            "BaseFont" => Object::Name(b"NJEQOD+Aptos".to_vec()),
            "Encoding" => Object::Reference(encoding_id),
        };

        let result = parse_font_encoding(&doc, &font_dict).expect("encoding should parse");

        assert_eq!(result.map.get(&0x88u8), Some(&'\u{FB00}'));
        assert_eq!(result.map.get(&0x89u8), Some(&'\u{FB01}'));
        assert_eq!(result.map.get(&0xADu8), Some(&'\u{FB02}'));
    }

    #[test]
    fn private_g431_does_not_map_for_unrelated_fonts() {
        let (doc, encoding_id) = doc_with_private_differences();
        let font_dict = dictionary! {
            "BaseFont" => Object::Name(b"ABCDEF+OtherFont".to_vec()),
            "Encoding" => Object::Reference(encoding_id),
        };

        let result = parse_font_encoding(&doc, &font_dict).expect("encoding should parse");

        assert!(!result.map.contains_key(&0x88u8));
        assert_eq!(result.map.get(&0x89u8), Some(&'\u{FB01}'));
        assert_eq!(result.map.get(&0xADu8), Some(&'\u{FB02}'));
    }

    #[test]
    fn cid_font_with_unparseable_cmap_does_not_emit_latin1_mojibake() {
        // Type0/CID font (font_widths reports `is_cid=true`) where the
        // ToUnicode CMap couldn't be parsed (FontCMaps doesn't have the
        // obj_num). Bytes are a 2-byte CID stream containing high bytes
        // that aren't valid UTF-8 — exactly the case in the production
        // samples (Identity-H text where the ToUnicode CMap was missing
        // or malformed, scrape_id 019de78c-..., e.g. "Í Ù Z)¿").
        //
        // Without the guard, the function falls through to the byte-by-byte
        // Latin-1 fallback and produces "ÍÙ" (U+00CD U+00D9). The correct
        // behavior is to emit U+FFFD per CID so downstream
        // `detect_encoding_issues` flags the page for OCR.
        let bytes = vec![0xCD_u8, 0xD9, 0xCD, 0xD9];
        let obj = Object::String(bytes, lopdf::StringFormat::Hexadecimal);

        let font_cmaps = FontCMaps::default();
        let mut font_tounicode_refs: HashMap<String, u32> = HashMap::new();
        font_tounicode_refs.insert("F0".to_string(), 999);
        let inline_cmaps = HashMap::new();
        let font_encodings: PageFontEncodings = HashMap::new();
        let encoding_cache: HashMap<String, Encoding<'_>> = HashMap::new();
        let mut decisions = CMapDecisionCache::new();
        let mut font_widths: PageFontWidths = HashMap::new();
        font_widths.insert("F0".to_string(), make_font_info(&[], 1000, true));

        let result = extract_text_from_operand(
            &obj,
            "F0",
            None,
            &font_cmaps,
            &font_tounicode_refs,
            &inline_cmaps,
            &font_encodings,
            &encoding_cache,
            &mut decisions,
            &font_widths,
        );

        let text = result.expect("CID font fallback should still emit a marker");
        assert!(
            !text.contains('\u{00CD}') && !text.contains('\u{00D9}'),
            "CID font with unparseable CMap leaked Latin-1 mojibake: {text:?}"
        );
        assert!(
            text.contains('\u{FFFD}'),
            "CID font with unparseable CMap should emit U+FFFD so detect_encoding_issues fires: {text:?}"
        );
    }

    #[test]
    fn simple_font_single_byte_fallback_passes_high_bytes_through() {
        // A Type1/TrueType simple font (is_cid=false) with a `/ToUnicode`
        // reference but no usable CMap and no `/Differences` map.
        // Per-byte fallback is the canonical interpretation here — these
        // bytes are character codes, not CIDs. The CID guard must NOT strip
        // them. Reproduces the false positive that an earlier version of the
        // guard introduced for fonts in PDFs like pdf-evals/Navigating-
        // Artificial-Intelligence-..., where bytes like 0xB6 are legitimate
        // single-byte character codes.
        let bytes = vec![0x24_u8, 0x47, 0xB6, 0x56]; // "$G¶V"
        let obj = Object::String(bytes, lopdf::StringFormat::Hexadecimal);

        let font_cmaps = FontCMaps::default();
        let mut font_tounicode_refs: HashMap<String, u32> = HashMap::new();
        font_tounicode_refs.insert("F1".to_string(), 999);
        let inline_cmaps = HashMap::new();
        let font_encodings: PageFontEncodings = HashMap::new();
        let encoding_cache: HashMap<String, Encoding<'_>> = HashMap::new();
        let mut decisions = CMapDecisionCache::new();
        let mut font_widths: PageFontWidths = HashMap::new();
        font_widths.insert("F1".to_string(), make_font_info(&[], 1000, false));

        let text = extract_text_from_operand(
            &obj,
            "F1",
            None,
            &font_cmaps,
            &font_tounicode_refs,
            &inline_cmaps,
            &font_encodings,
            &encoding_cache,
            &mut decisions,
            &font_widths,
        )
        .expect("simple font should round-trip Latin-1 bytes");
        assert_eq!(text, "$G\u{00B6}V");
        assert!(
            !text.contains('\u{FFFD}'),
            "simple font fallback must not stamp FFFD over legitimate bytes: {text:?}"
        );
    }

    #[test]
    fn simple_font_single_byte_fallback_maps_cp1252_punctuation() {
        let bytes = vec![b'l', 0x92_u8, b'a', b'c', b'a', b'd'];
        let obj = Object::String(bytes, lopdf::StringFormat::Hexadecimal);

        let font_cmaps = FontCMaps::default();
        let font_tounicode_refs: HashMap<String, u32> = HashMap::new();
        let inline_cmaps = HashMap::new();
        let font_encodings: PageFontEncodings = HashMap::new();
        let encoding_cache: HashMap<String, Encoding<'_>> = HashMap::new();
        let mut decisions = CMapDecisionCache::new();
        let font_widths: PageFontWidths = HashMap::new();

        let text = extract_text_from_operand(
            &obj,
            "F1",
            None,
            &font_cmaps,
            &font_tounicode_refs,
            &inline_cmaps,
            &font_encodings,
            &encoding_cache,
            &mut decisions,
            &font_widths,
        )
        .expect("simple font should decode CP1252 punctuation");

        assert_eq!(text, "l’acad");
    }

    #[test]
    fn cached_encoding_decode_normalizes_cp1252_controls() {
        let text = normalize_cp1252_controls("d\u{92}un \u{96} test".to_string(), true);
        assert_eq!(text, "d’un – test");
    }

    #[test]
    fn tex_font_decode_keeps_c1_ligature_bytes_unmodified() {
        let text = normalize_cp1252_controls("de\u{85}ciente \u{87}uid".to_string(), false);
        assert_eq!(text, "de\u{85}ciente \u{87}uid");
        assert!(!should_use_cp1252_single_byte_fallback(
            Some("TTdcr10"),
            false
        ));
        assert!(!should_use_cp1252_single_byte_fallback(
            Some("cmr10"),
            false
        ));
    }

    #[test]
    fn winansi_text_font_uses_cp1252_fallback() {
        assert!(should_use_cp1252_single_byte_fallback(
            Some("BJPQNQ+Times-Roman"),
            false
        ));
    }

    fn gid_font_doc(bfchar: Option<&str>) -> (Document, lopdf::ObjectId) {
        use lopdf::Stream;
        let mut doc = Document::with_version("1.4");
        let cmap = format!(
            "/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
1 begincodespacerange
<00> <FF>
endcodespacerange
1 beginbfchar
{}
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end",
            bfchar.unwrap_or_default()
        );
        let tounicode_id = doc.add_object(Object::Stream(Stream::new(
            dictionary! {},
            cmap.into_bytes(),
        )));
        let enc_id = doc.add_object(dictionary! {
            "Type" => "Encoding",
            "Differences" => vec![
                1.into(),
                Object::Name(b"gid1283".to_vec()),
                Object::Name(b"gid1464".to_vec()),
            ],
        });
        let mut font = dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => "ABCDEF+OpenSymbol",
            "Encoding" => Object::Reference(enc_id),
        };
        if bfchar.is_some() {
            font.set("ToUnicode", Object::Reference(tounicode_id));
        }
        let font_id = doc.add_object(font);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => Object::Reference(font_id) },
            },
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        let pages_id = doc.add_object(dictionary! {
            "Type" => "Pages",
            "Count" => Object::Integer(1),
            "Kids" => vec![Object::Reference(page_id)],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        (doc, page_id)
    }

    fn gid_flagged(bfchar: Option<&str>) -> bool {
        let (doc, page_id) = gid_font_doc(bfchar);
        let cmaps = FontCMaps::from_doc(&doc);
        let fonts = doc.get_page_fonts(page_id).unwrap();
        let (_, has_gid_fonts) = build_font_encodings(&doc, &fonts, &cmaps);
        has_gid_fonts
    }

    #[test]
    fn gid_differences_with_covering_tounicode_are_not_flagged() {
        // LibreOffice subsets write /gidNNNN Differences names alongside a
        // ToUnicode CMap that decodes those codes; the page must not be
        // flagged as unresolvable (which would suppress the whole document's
        // markdown when every page carries such a font).
        assert!(!gid_flagged(Some("<01> <2022>\n<02> <25E6>")));
    }

    #[test]
    fn gid_differences_with_partial_tounicode_are_not_flagged() {
        // An emoji ZWJ sequence maps whole on its first code; the remaining
        // component-glyph codes are subset leftovers, not damage.
        assert!(!gid_flagged(Some(
            "<01> <D83DDC68200DD83DDC69200DD83DDC67>"
        )));
    }

    #[test]
    fn gid_differences_without_tounicode_are_flagged() {
        assert!(
            gid_flagged(None),
            "gid glyphs without ToUnicode are unresolvable"
        );
    }

    #[test]
    fn gid_differences_with_disjoint_tounicode_are_flagged() {
        // A ToUnicode that never addresses the gid codes leaves them
        // unresolvable.
        assert!(gid_flagged(Some("<10> <0041>")));
    }

    #[test]
    fn gid_differences_with_replacement_char_tounicode_are_flagged() {
        // A mapping to U+FFFD is not usable — extraction rejects it as an
        // invalid CMap result — so it must not clear the gid flag.
        assert!(gid_flagged(Some("<01> <FFFD>\n<02> <FFFD>")));
    }

    #[test]
    fn parse_cid_w_array_range_and_consecutive() {
        use super::parse_cid_w_array;
        use lopdf::{Document, Object};
        use std::collections::HashMap;

        let doc = Document::new();
        let mut widths = HashMap::new();
        let w = vec![
            Object::Integer(10),
            Object::Integer(12),
            Object::Integer(500),
            Object::Integer(20),
            Object::Array(vec![Object::Integer(100), Object::Integer(200)]),
        ];
        parse_cid_w_array(&doc, &w, &mut widths);
        assert_eq!(widths.get(&10), Some(&500));
        assert_eq!(widths.get(&11), Some(&500));
        assert_eq!(widths.get(&12), Some(&500));
        assert_eq!(widths.get(&20), Some(&100));
        assert_eq!(widths.get(&21), Some(&200));
    }

    #[test]
    fn parse_cid_w_array_repeated_full_ranges_stay_bounded() {
        use super::parse_cid_w_array;
        use crate::tounicode::MAX_CID_W_EXPANSION;
        use lopdf::{Document, Object};
        use std::collections::HashMap;

        let doc = Document::new();
        let mut widths = HashMap::new();
        let mut w = Vec::new();
        for _ in 0..5_000 {
            w.push(Object::Integer(0));
            w.push(Object::Integer(65535));
            w.push(Object::Integer(500));
        }
        parse_cid_w_array(&doc, &w, &mut widths);
        assert!(widths.len() <= MAX_CID_W_EXPANSION);
        assert_eq!(widths.get(&0), Some(&500));
        assert_eq!(widths.get(&65535), Some(&500));
    }
}
