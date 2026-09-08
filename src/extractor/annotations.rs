//! `/FreeText` annotation content extraction.
//!
//! FreeText annotations (sticky-note-style text boxes anchored to a page,
//! distinct from the page's main content stream) carry their text via
//! `/Contents` and, usually, an `/AP` `/N` appearance stream that renders it.
//! Neither is walked by the content-stream extractor, so this text was
//! previously invisible to extraction.
//!
//! `/Contents` is preferred: it's the string PDF viewers expose to search
//! and accessibility tools, and it survives even when the appearance stream
//! is malformed or missing. The appearance stream is decoded as a fallback
//! only when `/Contents` is absent or empty, by reusing the existing Form
//! XObject text extractor with a computed appearance matrix (PDF 32000-1:2008
//! §12.5.5) so annotation text benefits from the same font/CMap decoding as
//! body text.

use lopdf::{Document, Object, ObjectId};

use crate::text_utils::decode_text_string;
use crate::tounicode::FontCMaps;
use crate::types::{ItemType, TextItem};

use super::fonts::{CMapDecisionCache, FontStyleCache};
use super::get_number;
use super::xobjects::{extract_form_xobject_text, FormWalkBudget};

/// Annotation `/F` flag bits (PDF 32000-1:2008 Table 165).
const ANNOT_FLAG_HIDDEN: i64 = 0x02;
const ANNOT_FLAG_NOVIEW: i64 = 0x20;

/// A `/FreeText` annotation collected from a page's `/Annots` array, with
/// just enough of its dictionary resolved to recover text either from
/// `/Contents` or by decoding its `/AP` `/N` appearance stream.
struct FreeTextAnnot {
    /// (x, y, width, height) in page space, matching `TextItem`'s convention.
    rect: (f32, f32, f32, f32),
    contents: Option<String>,
    ap_form_id: Option<ObjectId>,
}

fn resolve_dict_obj<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a lopdf::Dictionary> {
    if let Ok(obj_ref) = obj.as_reference() {
        doc.get_dictionary(obj_ref).ok()
    } else {
        obj.as_dict().ok()
    }
}

/// Collect `/FreeText` annotations from a page's `/Annots` array.
fn collect_page_freetext_annots(doc: &Document, page_id: ObjectId) -> Vec<FreeTextAnnot> {
    let mut out = Vec::new();
    let Ok(page_dict) = doc.get_dictionary(page_id) else {
        return out;
    };

    let annots = match page_dict.get(b"Annots") {
        Ok(annots_ref) => {
            if let Ok(obj_ref) = annots_ref.as_reference() {
                doc.get_object(obj_ref)
                    .ok()
                    .and_then(|o| o.as_array().ok().cloned())
            } else {
                annots_ref.as_array().ok().cloned()
            }
        }
        Err(_) => None,
    };
    let Some(annots) = annots else {
        return out;
    };

    for annot_ref in annots {
        let annot_dict = if let Ok(obj_ref) = annot_ref.as_reference() {
            doc.get_dictionary(obj_ref).ok()
        } else {
            annot_ref.as_dict().ok()
        };
        let Some(annot_dict) = annot_dict else {
            continue;
        };

        let is_freetext = annot_dict
            .get(b"Subtype")
            .ok()
            .and_then(|s| s.as_name().ok())
            .is_some_and(|name| name == b"FreeText");
        if !is_freetext {
            continue;
        }

        let flags = annot_dict
            .get(b"F")
            .ok()
            .and_then(get_number)
            .map(|f| f as i64)
            .unwrap_or(0);
        if flags & (ANNOT_FLAG_HIDDEN | ANNOT_FLAG_NOVIEW) != 0 {
            continue;
        }

        let Ok(rect_obj) = annot_dict.get(b"Rect") else {
            continue;
        };
        let Ok(rect_array) = rect_obj.as_array() else {
            continue;
        };
        if rect_array.len() < 4 {
            continue;
        }
        let x1 = get_number(&rect_array[0]).unwrap_or(0.0);
        let y1 = get_number(&rect_array[1]).unwrap_or(0.0);
        let x2 = get_number(&rect_array[2]).unwrap_or(0.0);
        let y2 = get_number(&rect_array[3]).unwrap_or(0.0);
        let rect = (x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs());

        let contents = annot_dict
            .get(b"Contents")
            .ok()
            .and_then(|c| c.as_str().ok())
            .map(decode_text_string)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // `/N` is normally a direct reference to a Form XObject stream. When
        // it resolves to a sub-dictionary keyed by appearance state (the
        // checkbox/radio-button shape), that doesn't apply to FreeText
        // annotations — skip rather than guess a state.
        let ap_form_id = annot_dict
            .get(b"AP")
            .ok()
            .and_then(|ap| resolve_dict_obj(doc, ap))
            .and_then(|ap_dict| ap_dict.get(b"N").ok())
            .and_then(|n| n.as_reference().ok())
            .filter(|&id| matches!(doc.get_object(id), Ok(Object::Stream(_))));

        out.push(FreeTextAnnot {
            rect,
            contents,
            ap_form_id,
        });
    }

    out
}

/// Compute the CTM that maps a `/FreeText` annotation's `/AP` `/N` appearance
/// stream content into page space, per PDF 32000-1:2008 §12.5.5: transform
/// the form's `/BBox` corners by its own `/Matrix`, take the axis-aligned
/// bounding box of the result, then compute the matrix `A` that maps that
/// bounding box onto the annotation's `/Rect`.
///
/// The caller passes the returned matrix as `extract_form_xobject_text`'s
/// `parent_ctm`; that function applies the form's own `/Matrix` on top of it,
/// reproducing the spec's `Matrix * A` composition.
fn appearance_matrix(
    doc: &Document,
    ap_form_id: ObjectId,
    // (x, y, width, height), matching `FreeTextAnnot::rect`'s convention.
    rect: (f32, f32, f32, f32),
) -> Option<[f32; 6]> {
    let Ok(Object::Stream(stream)) = doc.get_object(ap_form_id) else {
        return None;
    };
    let bbox = stream.dict.get(b"BBox").ok()?.as_array().ok()?;
    if bbox.len() < 4 {
        return None;
    }
    let bx0 = get_number(&bbox[0])?;
    let by0 = get_number(&bbox[1])?;
    let bx1 = get_number(&bbox[2])?;
    let by1 = get_number(&bbox[3])?;

    let matrix = stream
        .dict
        .get(b"Matrix")
        .ok()
        .and_then(|m| m.as_array().ok())
        .filter(|arr| arr.len() >= 6)
        .map(|arr| {
            let mut m = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];
            for (i, v) in arr.iter().take(6).enumerate() {
                m[i] = get_number(v).unwrap_or(if i == 0 || i == 3 { 1.0 } else { 0.0 });
            }
            m
        })
        .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    // Transform the four BBox corners by Matrix and take their axis-aligned
    // bounding box (Matrix may rotate/skew, so the transformed shape need
    // not be axis-aligned).
    let corners = [(bx0, by0), (bx1, by0), (bx1, by1), (bx0, by1)];
    let transformed: Vec<(f32, f32)> = corners
        .iter()
        .map(|&(u, v)| super::apply_ctm_point(&matrix, u, v))
        .collect();
    let tx0 = transformed
        .iter()
        .map(|p| p.0)
        .fold(f32::INFINITY, f32::min);
    let tx1 = transformed
        .iter()
        .map(|p| p.0)
        .fold(f32::NEG_INFINITY, f32::max);
    let ty0 = transformed
        .iter()
        .map(|p| p.1)
        .fold(f32::INFINITY, f32::min);
    let ty1 = transformed
        .iter()
        .map(|p| p.1)
        .fold(f32::NEG_INFINITY, f32::max);

    let (rx, ry, rw, rh) = rect;
    let (bw, bh) = (tx1 - tx0, ty1 - ty0);
    if bw.abs() < f32::EPSILON || bh.abs() < f32::EPSILON {
        return None;
    }
    let sx = rw / bw;
    let sy = rh / bh;
    Some([sx, 0.0, 0.0, sy, rx - tx0 * sx, ry - ty0 * sy])
}

/// Extract `/FreeText` annotation text from a page's `/Annots` array as
/// `TextItem`s positioned at each annotation's `/Rect`, tagged
/// `ItemType::Annotation`.
pub(crate) fn extract_page_annotation_text(
    doc: &Document,
    page_id: ObjectId,
    page_num: u32,
    font_cmaps: &FontCMaps,
    style_cache: &mut FontStyleCache,
) -> Vec<TextItem> {
    let annots = collect_page_freetext_annots(doc, page_id);
    if annots.is_empty() {
        return Vec::new();
    }

    let mut items = Vec::new();
    for annot in annots {
        let (x, y, width, height) = annot.rect;

        if let Some(text) = annot.contents {
            items.push(TextItem {
                text,
                x,
                y,
                width,
                height,
                font: String::new(),
                font_size: 0.0,
                page: page_num,
                is_bold: false,
                is_italic: false,
                is_underline: false,
                is_strikeout: false,
                item_type: ItemType::Annotation,
                mcid: None,
            });
            continue;
        }

        let Some(ap_form_id) = annot.ap_form_id else {
            continue;
        };
        let Some(ctm) = appearance_matrix(doc, ap_form_id, (x, y, width, height)) else {
            continue;
        };

        // A fresh, small traversal budget per annotation: appearance streams
        // are tiny by construction (they render one text box), and bounding
        // per-annotation keeps a crafted document with many annotations from
        // compounding a single page-wide budget.
        let mut cmap_decisions = CMapDecisionCache::new();
        let mut budget = FormWalkBudget::new();
        let (form_items, _) = extract_form_xobject_text(
            doc,
            ap_form_id,
            page_num,
            font_cmaps,
            &ctm,
            &mut cmap_decisions,
            style_cache,
            &mut budget,
        );
        for mut item in form_items {
            if item.text.trim().is_empty() {
                continue;
            }
            item.item_type = ItemType::Annotation;
            item.mcid = None;
            items.push(item);
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object};

    #[test]
    fn freetext_contents_extracted_at_rect_position() {
        let mut doc = Document::new();
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "FreeText",
            "Contents" => Object::string_literal("Reviewer note\r"),
            "Rect" => vec![100.into(), 200.into(), 300.into(), 240.into()],
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(annot_id)],
        });

        let font_cmaps = FontCMaps::from_doc(&doc);
        let mut style_cache = FontStyleCache::new();
        let items = extract_page_annotation_text(&doc, page_id, 1, &font_cmaps, &mut style_cache);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Reviewer note");
        assert_eq!(items[0].x, 100.0);
        assert_eq!(items[0].y, 200.0);
        assert_eq!(items[0].width, 200.0);
        assert_eq!(items[0].height, 40.0);
        assert!(matches!(items[0].item_type, ItemType::Annotation));
    }

    #[test]
    fn non_freetext_annotations_are_ignored() {
        let mut doc = Document::new();
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Square",
            "Contents" => Object::string_literal("not a note"),
            "Rect" => vec![100.into(), 200.into(), 300.into(), 240.into()],
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(annot_id)],
        });

        let font_cmaps = FontCMaps::from_doc(&doc);
        let mut style_cache = FontStyleCache::new();
        let items = extract_page_annotation_text(&doc, page_id, 1, &font_cmaps, &mut style_cache);
        assert!(items.is_empty());
    }

    #[test]
    fn hidden_freetext_annotation_is_skipped() {
        let mut doc = Document::new();
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "FreeText",
            "F" => ANNOT_FLAG_HIDDEN,
            "Contents" => Object::string_literal("hidden note"),
            "Rect" => vec![100.into(), 200.into(), 300.into(), 240.into()],
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(annot_id)],
        });

        let font_cmaps = FontCMaps::from_doc(&doc);
        let mut style_cache = FontStyleCache::new();
        let items = extract_page_annotation_text(&doc, page_id, 1, &font_cmaps, &mut style_cache);
        assert!(items.is_empty());
    }

    #[test]
    fn empty_contents_falls_back_to_appearance_stream() {
        let mut doc = Document::new();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let ap_stream = lopdf::Stream::new(
            dictionary! {
                "Subtype" => "Form",
                "FormType" => 1,
                "BBox" => vec![0.into(), 0.into(), 200.into(), 40.into()],
                "Resources" => dictionary! {
                    "Font" => dictionary! {
                        "F1" => Object::Reference(font_id),
                    },
                },
            },
            b"BT /F1 12 Tf 5 15 Td (from appearance) Tj ET".to_vec(),
        );
        let ap_form_id = doc.add_object(Object::Stream(ap_stream));
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "FreeText",
            "Rect" => vec![100.into(), 200.into(), 300.into(), 240.into()],
            "AP" => dictionary! {
                "N" => Object::Reference(ap_form_id),
            },
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(annot_id)],
        });

        let font_cmaps = FontCMaps::from_doc(&doc);
        let mut style_cache = FontStyleCache::new();
        let items = extract_page_annotation_text(&doc, page_id, 1, &font_cmaps, &mut style_cache);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text.trim(), "from appearance");
        assert!(matches!(items[0].item_type, ItemType::Annotation));
        // The appearance's BBox origin maps to the Rect origin 1:1 (same
        // width/height, identity Matrix), so the glyph position lands inside
        // the Rect rather than at some unrelated coordinate.
        assert!(items[0].x >= 100.0 && items[0].x <= 300.0);
        assert!(items[0].y >= 200.0 && items[0].y <= 240.0);
    }
}
