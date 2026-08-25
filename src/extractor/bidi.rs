//! Phase 3: bidi classification + pen-geometry cross-check detector for
//! Instance 2 (silent visual-order word reversal, no structural signal).
//!
//! Ports two pieces of MuPDF's own mechanism onto Phase 2's per-glyph pen
//! positions (see the Phase 3 research notes for the full source trace):
//!
//! 1. `guess_bidi_level` (`source/pdf/pdf-op-run.c`) — classify each
//!    glyph's first decoded Unicode codepoint via `unicode_bidi::bidi_class`
//!    (real Unicode Bidi_Class data, not hand-rolled), then a direct port
//!    of MuPDF's simplified level-assignment switch. This is a forward-pass
//!    heuristic, NOT full UAX #9 resolution — exactly what MuPDF itself
//!    does (its own complete UAX #9 port, `source/fitz/bidi.c`, is used
//!    only for HTML/EPUB reflow, never for PDF text extraction).
//! 2. The geometry hypothesis test (`source/fitz/stext-device.c`,
//!    `fz_add_stext_char_imp`) — for each RTL-classified glyph, compare
//!    its REAL pen movement (Phase 2's `GlyphDecode::pen`) against two
//!    competing hypotheses: "moved backward by about its own width"
//!    (already correct logical order) vs. "moved forward by about its own
//!    width" (visual order, needs reversal) — and the deferred reversal
//!    pass (`reverse_bidi_line`): once flagged, maximal runs of
//!    consecutive flagged glyphs get reversed in place.
//!
//! ## Scope decision: within one pre-merge item, not across the whole page
//!
//! MuPDF's own model has no concept of "items" at all — `pdf_show_char` is
//! called once per glyph, in raw content-stream order, for the WHOLE page,
//! and `fz_add_stext_char_imp`'s pen/lag_pen state threads continuously
//! across that entire stream. pdf-inspector's architecture is
//! fundamentally different: `content_stream.rs`/`xobjects.rs` already
//! group each Tj/TJ/`'` operand's glyphs into one `TextItem` before
//! `merge_text_items_with_glyphs` ever runs, and that function's own
//! line-grouping/merging logic (Bug #1's fix) decides which ITEMS join
//! into one run using x-adjacency on already-sorted items, not raw stream
//! order — running this module's geometry check on that sorted order would
//! make the check circular (sorting by x already imposes an order,
//! independent of whether the underlying stream order was correct).
//!
//! This module instead runs the bidi+geometry pass WITHIN each pre-merge
//! item's own glyph list — i.e. within one `decode_operand_glyphs` call's
//! worth of CIDs, in their genuine stream order — BEFORE any inter-item
//! merging happens. This is directly analogous to Bug #2's `/ReversedChars`
//! fix and the `/TagSuspect` corpus finding (both are also
//! within-one-operand reversals — see the Instance 2 investigation), and
//! keeps the change tractable and low-risk: no need to reassign glyphs
//! across item boundaries, no change to the existing (untouched) x-sort+
//! merge arithmetic. Reversal itself never crosses an item boundary. The
//! honest limitation: a reversal that spans MULTIPLE separate Tj/TJ calls
//! on the same line will NOT be caught by this phase — see the
//! verification report for how much of the real corpus this scope
//! decision actually covers.
//!
//! ### Pen continuity across item boundaries — tried, reverted
//!
//! `apply_bidi_reversal` takes a `PenContinuity` parameter that COULD be
//! threaded from one item's ending state into the next item's starting
//! state (both are available, in genuine parse/stream order, at the call
//! site in `merge_text_items_with_glyphs`). This was tried as a fix for a
//! real problem: some real-world PDF producers (confirmed: the
//! Chromium/Skia PDF backend used by this project's own
//! Playwright-generated reference fixtures) emit one already-correct RTL
//! clause as many tiny consecutive Tj/TJ operands (2-6 glyphs each)
//! instead of one long run, and cross-checking against real MuPDF (via
//! PyMuPDF) confirmed it extracts the SAME fixture correctly — because its
//! glyph stream is genuinely continuous page-wide, it never loses the
//! context a cold per-item reset throws away.
//!
//! However, threading the FULL state (including `cur_bidi`/`last_bidi`,
//! needed for RTL-run classification) across the boundary caused a
//! regression worse than what it fixed: a NEUTRAL glyph (e.g. a plain
//! space) sitting at the very start of item N would inherit item
//! (N-1)'s trailing RTL classification via `guess_bidi_level`'s
//! `cur_bidi`-inheritance rule for weak/neutral classes, get swept into
//! item N's OWN (correctly within-item-scoped) reversal, and end up
//! relocated to the wrong end of item N's own output — silently eating
//! the word-separating space between item N-1's and item N's
//! independently-reversed text (confirmed via corpus diff: "نموذج كراسة"
//! became "نموذجكراسة"). This is the "reversal never crosses item
//! boundaries" design constraint asserting itself: item N-1 had already
//! finished emitting its own reversed output by the time item N's carried
//! classification pretended they were still one continuous span — the
//! classification carried continuity the reversal itself couldn't honor.
//! It also did not fix the Chromium-fragment false positives it was
//! trying to address (those come from genuine per-pair geometric noise in
//! short runs, not from an untestable first unit — see the verification
//! report). Given it fixed nothing and broke something that worked, it
//! was reverted: every call site passes a fresh `PenContinuity::default()`
//! per item (equivalent to a permanent cold start). The parameter stays
//! in the signature so a future, more careful attempt (e.g. carrying pen
//! geometry but never bidi classification across the boundary) doesn't
//! have to redo this wiring — but doing so needs its own verification
//! pass, not a repeat of this one.
//!
//! ## A genuine unit mismatch (not a tuning knob)
//!
//! MuPDF's `adv` parameter (`fz_add_stext_char_imp`) is the glyph's advance
//! in TEXT SPACE — a fraction of one em (e.g. ~0.5), independent of font
//! size — added directly to `logical_delta / size` where
//! `size = fz_matrix_expansion(trm)` (a device-space scale factor). Both
//! terms end up in "em fraction" units before the `SPACE_DIST = 0.15`
//! comparison.
//!
//! pdf-inspector's `GlyphDecode::full_advance_ts` (Phase 3 addition) is in
//! PAGE-SPACE units — font_size and CTM scaling already baked in (matching
//! `.pen`'s own units), NOT em-fraction. To compare against MuPDF's
//! documented threshold constants ("as multiples of font size" — confirmed
//! from the constants' own doc comments), this module divides EVERY term
//! — including the advance — by the run's own `font_size`, so the whole
//! formula ends up in the same "multiples of font size" units the
//! constants assume. This is a structural consequence of the two tools'
//! different internal unit conventions, not a tuned/derived value — the
//! threshold CONSTANTS themselves (`SPACE_DIST`/`SPACE_MAX_DIST`/
//! `BASE_MAX_DIST`) are used exactly as MuPDF documents them, unchanged.
//!
//! ## Direction vector simplification
//!
//! MuPDF computes `ndir` per glyph from that glyph's own text rendering
//! matrix, correctly handling rotated/sheared text. pdf-inspector already
//! normalizes whole-page rotation via `content_stream::correct_rotated_page`
//! BEFORE `merge_text_items_with_glyphs` runs, so by the time this module
//! sees glyph positions, the page's coordinate space is already
//! horizontal-reading. This module assumes `ndir = (1, 0)` — a deliberate
//! simplification justified by that upstream normalization, not an
//! oversight. A page with in-content shearing (e.g. an italic transform
//! applied via a mid-page `cm`, distinct from the whole-page rotation
//! `correct_rotated_page` handles) is not accounted for.
//!
//! ## `/ReversedChars` and `/TagSuspect` stay in place
//!
//! Both existing tag-based fixes run in `content_stream.rs`, reversing raw
//! CID bytes BEFORE `decode_operand_glyphs` (and hence this module) ever
//! sees them. This module never special-cases those tags — by the time it
//! runs on already-tag-corrected glyphs, the geometry should already look
//! like hypothesis A (correct logical order) and nothing gets re-flagged.
//! That is the intended cross-check: two independent mechanisms (a
//! structural tag and a geometric heuristic) agreeing on already-correct
//! content, not a replacement of one by the other.

use super::fonts::GlyphDecode;
use unicode_bidi::{bidi_class, BidiClass};

// MuPDF's own documented constants (`stext-device.c`), as multiples of font
// size — used exactly as-is, not tuned against this project's corpus.
const SPACE_DIST: f32 = 0.15;
const SPACE_MAX_DIST: f32 = 0.8;
const BASE_MAX_DIST: f32 = 0.8;

/// Direct port of MuPDF's `guess_bidi_level` (`pdf-op-run.c`) — a
/// forward-pass heuristic bidi LEVEL assignment, NOT full UAX #9
/// resolution. `cur_bidi` is the previous glyph's own resolved level
/// (0 = LTR, 1 = RTL), mirroring `pdf_run_processor`'s persistent
/// `pr->bidi` field (here, reset per item — see module docs).
fn guess_bidi_level(class: BidiClass, cur_bidi: u8) -> u8 {
    match class {
        // strong
        BidiClass::L => 0,
        BidiClass::R => 1,
        BidiClass::AL => 1,
        // weak
        BidiClass::EN | BidiClass::ES | BidiClass::ET => 0,
        BidiClass::AN => 1,
        BidiClass::CS | BidiClass::NSM | BidiClass::BN => cur_bidi,
        // neutral
        BidiClass::B | BidiClass::S | BidiClass::WS | BidiClass::ON => cur_bidi,
        // embedding, override, pop ... MuPDF doesn't support them either
        // (its own comment: "we don't support them") — falls to its
        // `default: return 0;` case.
        BidiClass::LRE
        | BidiClass::LRI
        | BidiClass::LRO
        | BidiClass::PDF
        | BidiClass::PDI
        | BidiClass::RLE
        | BidiClass::RLI
        | BidiClass::RLO
        | BidiClass::FSI => 0,
    }
}

/// One "logical unit" for bidi/geometry purposes: a real glyph (its own
/// CID, pen position, advance) plus any ligature filler glyphs that follow
/// it (see `GlyphDecode`'s own doc on fillers) — treated as one atomic
/// block so reversal never separates a ligature's characters. `indices`
/// are positions into the original glyph slice, in original order (real
/// glyph first, then fillers) — always a contiguous range by construction.
struct Unit {
    indices: Vec<usize>,
}

fn group_into_units(glyphs: &[GlyphDecode]) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut i = 0;
    while i < glyphs.len() {
        if glyphs[i].code_count == 0 {
            // A filler with no preceding real glyph in this slice shouldn't
            // happen for decode_operand_glyphs output (fillers only ever
            // follow their own real glyph in the same call) — but stay
            // defensive rather than panic on malformed input.
            units.push(Unit { indices: vec![i] });
            i += 1;
            continue;
        }
        let mut indices = vec![i];
        let mut j = i + 1;
        while j < glyphs.len() && glyphs[j].code_count == 0 {
            indices.push(j);
            j += 1;
        }
        units.push(Unit { indices });
        i = j;
    }
    units
}

/// Pen/bidi continuity state for one `apply_bidi_reversal` call. Every call
/// site currently passes a fresh `PenContinuity::default()` (a cold start,
/// matching this module's original per-item scope) — see module docs, "Pen
/// continuity across item boundaries — tried, reverted", for why threading
/// this from one item's ending state into the next item's starting state
/// was tried and reverted, and why the parameter stays in the signature
/// anyway. Read-only with respect to reversal either way: this only ever
/// supplies `lag_pen`/`pen_end`/`cur_bidi`/`last_bidi` as the "previous
/// unit" for the first unit tested; it never lets a reversal reach outside
/// the glyph slice it was given.
#[derive(Default, Clone, Copy)]
pub(crate) struct PenContinuity {
    cur_bidi: u8,
    lag_pen: Option<(f32, f32)>,
    pen_end: Option<(f32, f32)>,
    last_bidi: u8,
}

/// Runs the bidi classification + pen-geometry hypothesis test over one
/// pre-merge item's glyphs (see module docs for the within-item scope
/// decision), flagging maximal runs of "visually stored" units and
/// reversing them in place. Returns `true` if any reversal happened.
/// `carry` supplies continuity from the previous item's own last unit
/// (module docs) and is updated in place from this item's own last unit,
/// for the NEXT call to use — reversal itself never crosses the boundary.
///
/// `font_size` is the item's own rendered font size — this module's analog
/// of MuPDF's `size = fz_matrix_expansion(trm)` (see module docs on why
/// the advance term needs the page-space-units correction).
pub(crate) fn apply_bidi_reversal(
    glyphs: &mut [GlyphDecode],
    font_size: f32,
    carry: &mut PenContinuity,
) -> bool {
    if glyphs.is_empty() || font_size <= 0.0 {
        *carry = PenContinuity::default();
        return false;
    }

    let units = group_into_units(glyphs);

    if std::env::var("BIDI_DEBUG").is_ok() {
        let before: String = glyphs.iter().map(|g| g.text.as_str()).collect();
        eprintln!("--- item start (font_size={font_size}) text_before={before:?}");
    }

    // `signal[i]` is MuPDF's `ch->bidi == 3` ("mark line as visual") —
    // a per-PAIR hint, not a per-unit reversal decision. MuPDF's own
    // `reverse_bidi_line` does NOT reverse only the individually-flagged
    // characters: once ANY character in a line has bidi==3
    // (`fixup_bboxes_and_bidi`'s `reorder` flag), `reverse_bidi_line`
    // reverses every MAXIMAL run of consecutive non-zero-bidi characters
    // in that line — including characters whose own pair test landed on
    // hypothesis A, "missing space", "overlap", or was never tested at all
    // (a run's first character, or one after a mixed-direction jump).
    // Reversing only individually-flagged pairs — the original version of
    // this function — fragments genuine reversed runs into disconnected
    // pieces (confirmed via BIDI_DEBUG tracing: a real Aspose-reversed
    // run's first letter, right after the space→RTL transition, can never
    // itself be geometry-tested, so it was left out of the old per-pair
    // reversal and the word came out rotated instead of fixed). This is
    // the same mechanism as the old function's "boundary correction" hack,
    // generalized correctly and ported from the real MuPDF control flow
    // instead of re-derived: `signal` below is `ch->bidi==3`'s trigger,
    // and `bidi_bits`/`run_break` below reconstruct which contiguous span
    // reverse_bidi_line would treat as one run.
    let mut signal: Vec<bool> = vec![false; units.len()];
    // Every unit's own RTL-ness bit, including unit 0 (which never gets a
    // geometry comparison but still participates in run membership).
    let mut bidi_bits: Vec<u8> = Vec::with_capacity(units.len());
    // `run_break[i]`: unit i's own geometry test hit the "large/unexpected
    // jump" case — MuPDF starts a NEW LINE here (`new_line = 1`), so unit i
    // does not join the same reversal run as the unit before it, even if
    // both are RTL-classified.
    let mut run_break: Vec<bool> = vec![false; units.len()];

    // Seeded from `carry` (the previous item's own last unit), not a cold
    // start — see module docs on why this crosses item boundaries.
    let mut cur_bidi: u8 = carry.cur_bidi;
    let mut lag_pen: Option<(f32, f32)> = carry.lag_pen; // dev->lag_pen
    let mut pen_end: Option<(f32, f32)> = carry.pen_end; // dev->pen
    let mut last_bidi: u8 = carry.last_bidi;

    for (u_idx, unit) in units.iter().enumerate() {
        let real = &glyphs[unit.indices[0]];
        let Some(p) = real.pen else {
            // No position available (shouldn't happen once pen_track_glyphs
            // has run, but stay defensive) — skip this unit's geometry
            // test and reset continuity rather than guess.
            lag_pen = None;
            pen_end = None;
            bidi_bits.push(last_bidi);
            continue;
        };

        let class = real
            .text
            .chars()
            .next()
            .map(bidi_class)
            .unwrap_or(BidiClass::ON);
        let level = guess_bidi_level(class, cur_bidi);
        cur_bidi = level;
        let bidi = level & 1;
        bidi_bits.push(bidi);

        // This unit's own advance in "multiples of font size" — see module
        // docs on the unit mismatch this division corrects for.
        let advance = real.full_advance_ts / font_size;
        // ndir = (1, 0): this unit's own end, in the same simplified
        // horizontal-direction convention as the rest of this module.
        let q = (p.0 + real.full_advance_ts, p.1);

        if let (Some(lag), Some(prev_end)) = (lag_pen, pen_end) {
            let delta = (p.0 - prev_end.0, p.1 - prev_end.1);
            let spacing = delta.0 / font_size; // ndir=(1,0): ndir·delta == delta.x
            let base_offset = delta.1 / font_size; // -ndir.y*dx + ndir.x*dy == dy

            if base_offset.abs() < BASE_MAX_DIST {
                if bidi != last_bidi {
                    // Mixed-direction jump — MuPDF ignores it (no reversal
                    // decision either way for this transition). Run
                    // continuity is already broken by the bidi_bits change
                    // itself, so no explicit run_break needed here.
                } else if bidi == 1 {
                    let logical_delta_x = p.0 - lag.0;
                    let logical_spacing = logical_delta_x / font_size + advance;

                    if logical_spacing.abs() < SPACE_DIST {
                        // Hypothesis A: logical (already correct) order.
                    } else if spacing.abs() < SPACE_DIST {
                        // Hypothesis B: visual order — signal this run.
                        signal[u_idx] = true;
                    } else if logical_spacing < 0.0 && logical_spacing > -SPACE_MAX_DIST {
                        // Probably a missing space (MuPDF's add_space path)
                        // — not a reversal signal, nothing to do here.
                    } else if spacing < 0.0 && spacing > -SPACE_MAX_DIST {
                        // Overlapping glyphs — not a reversal signal.
                    } else if spacing > 0.0 && spacing < SPACE_MAX_DIST {
                        signal[u_idx] = true;
                    } else {
                        // Large/unexpected jump — MuPDF starts a new line
                        // here (new_line = 1), which ends the current
                        // reversal run: this unit does not join the run
                        // the previous unit belonged to.
                        run_break[u_idx] = true;
                    }
                }
                // LTR/neutral glyphs: MuPDF's own else-branch only feeds its
                // add_space/new_line heuristics, which this module doesn't
                // reimplement (pdf-inspector's existing merge logic already
                // owns word-spacing/line-breaking decisions) — no bidi
                // consequence either way.
            } else {
                // base_offset too large — MuPDF treats this as a new
                // line/paragraph, ending the current reversal run here too.
                run_break[u_idx] = true;
            }
        }

        lag_pen = Some(p);
        pen_end = Some(q);
        last_bidi = bidi;
    }

    // Hand this item's own ending state to the NEXT item's call — see
    // module docs on cross-item pen continuity. This happens regardless of
    // whether a reversal occurs below; it reflects this item's REAL
    // (post-reversal-irrelevant — reversal doesn't change any pen
    // position, only `.text`) glyph geometry either way.
    *carry = PenContinuity {
        cur_bidi,
        lag_pen,
        pen_end,
        last_bidi,
    };

    if std::env::var("BIDI_DEBUG").is_ok() {
        eprintln!("bidi_bits={bidi_bits:?} signal={signal:?} run_break={run_break:?}");
    }

    // Deferred reversal pass — port of reverse_bidi_line + the `reorder`
    // check in fixup_bboxes_and_bidi. Find maximal runs of consecutive
    // RTL-classified units (bidi_bits[i] == 1), split at any run_break; a
    // whole run reverses if ANY unit inside it carries `signal`, exactly
    // mirroring MuPDF: `reorder` triggers on any ch->bidi==3 in the line,
    // and reverse_bidi_line then reverses every maximal non-zero-bidi span
    // regardless of which specific characters within it were the ones
    // that individually tested as hypothesis B.
    //
    // A stricter "require every testable pair in the run to signal, for
    // short runs" gate was tried here (verification report has the full
    // numbers) to filter out short-run false positives on
    // fragmented-item PDF producers (module docs, "Pen continuity" —
    // Chromium/Skia). It measurably reduced them, but the two
    // populations' (run length, signal density) distributions genuinely
    // overlap in the real 24-document Etimad corpus: some documents have
    // genuine, correctly-broken runs as short as length 5 with a
    // non-unanimous signal (verified via BIDI_DEBUG across multiple
    // documents, not just one), which is inside the same length range
    // (2-9) where the known_good fixtures' false positives live. No
    // length/density cutoff can separate them without either still
    // admitting false positives or silently un-fixing genuine short
    // Etimad runs — confirmed empirically, not assumed. Reverted; see the
    // verification report for the decision on how to proceed.
    let mut reversed_any = false;
    let mut i = 0;
    while i < bidi_bits.len() {
        if bidi_bits[i] == 1 {
            let mut j = i;
            while j + 1 < bidi_bits.len() && bidi_bits[j + 1] == 1 && !run_break[j + 1] {
                j += 1;
            }
            let has_signal = (i..=j).any(|k| signal[k]);
            if std::env::var("BIDI_DEBUG").is_ok() {
                let signal_count = (i..=j).filter(|&k| signal[k]).count();
                eprintln!(
                    "run [{i}..={j}] len={} signal_count={signal_count} would_reverse={}",
                    j - i + 1,
                    has_signal && j > i
                );
            }
            if has_signal && j > i {
                reverse_units(glyphs, &units[i..=j]);
                reversed_any = true;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }

    reversed_any
}

/// Reverses a maximal flagged run at the UNIT level: units swap order, but
/// each unit's own internal glyph order (real glyph then fillers) is
/// preserved, so a ligature's characters are never separated. `units` are
/// contiguous in the original glyph slice by construction (`group_into_
/// units` walks glyphs without gaps), so this is a straightforward
/// in-place block rewrite.
fn reverse_units(glyphs: &mut [GlyphDecode], units: &[Unit]) {
    let start = units[0].indices[0];
    let end = *units.last().unwrap().indices.last().unwrap();

    let mut new_order: Vec<GlyphDecode> = Vec::with_capacity(end - start + 1);
    for unit in units.iter().rev() {
        for &idx in &unit.indices {
            new_order.push(glyphs[idx].clone());
        }
    }
    glyphs[start..=end].clone_from_slice(&new_order);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(text: &str, pen_x: f32, pen_y: f32, advance: f32) -> GlyphDecode {
        GlyphDecode {
            text: text.to_string(),
            width_ts: advance,
            cid: Some(0),
            code_count: 1,
            space_count: 0,
            pen: Some((pen_x, pen_y)),
            full_advance_ts: advance,
        }
    }

    #[test]
    fn visual_order_rtl_run_gets_reversed() {
        // Correct word (read right-to-left): "بيت" (bayt, "house") = ب ي ت.
        // Stored in VISUAL order — as if the producer wrote the reversed
        // string "تيب" and positioned each successive glyph moving
        // FORWARD (increasing x) by about one glyph-width, mimicking LTR
        // layout. font_size == advance so every ratio in the formula is a
        // clean 1.0 — spacing (forward motion, hypothesis B) should land
        // well within SPACE_DIST=0.15 of 0, and logical_spacing
        // (backward-motion hypothesis) should NOT.
        let font_size = 10.0;
        let mut glyphs = vec![
            glyph("ت", 100.0, 700.0, 10.0),
            glyph("ي", 110.0, 700.0, 10.0),
            glyph("ب", 120.0, 700.0, 10.0),
        ];

        let mut carry = PenContinuity::default();
        let reversed = apply_bidi_reversal(&mut glyphs, font_size, &mut carry);

        assert!(
            reversed,
            "forward-moving RTL glyphs must be flagged as visual order"
        );
        let text: String = glyphs.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(text, "بيت", "reversal must recover the correct word");
    }

    #[test]
    fn logical_order_rtl_run_is_left_alone() {
        // The SAME word, "بيت", now stored CORRECTLY: each successive
        // glyph moves BACKWARD (decreasing x) by about one glyph-width —
        // genuine logical RTL order, exactly what Bug #1's own fix
        // already produces for correctly-authored RTL text.
        let font_size = 10.0;
        let mut glyphs = vec![
            glyph("ب", 120.0, 700.0, 10.0),
            glyph("ي", 110.0, 700.0, 10.0),
            glyph("ت", 100.0, 700.0, 10.0),
        ];

        let mut carry = PenContinuity::default();
        let reversed = apply_bidi_reversal(&mut glyphs, font_size, &mut carry);

        assert!(
            !reversed,
            "backward-moving (correct) RTL glyphs must not be flagged"
        );
        let text: String = glyphs.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(text, "بيت", "already-correct text must be untouched");
    }

    #[test]
    fn ligature_filler_stays_attached_to_its_real_glyph_through_reversal() {
        // Two RTL "words" — each really just one unit here — where the
        // second unit is a real glyph + filler (as Phase 1/2's ligature
        // handling produces), stored in visual (forward-moving) order.
        // The filler must travel WITH its real glyph, not get separated
        // or independently reordered.
        let font_size = 10.0;
        let mut glyphs = vec![
            glyph("ت", 100.0, 700.0, 10.0),
            GlyphDecode {
                text: "لا".chars().next().unwrap().to_string(), // "ل" (real half of the ligature)
                width_ts: 10.0,
                cid: Some(0x01D9),
                code_count: 1,
                space_count: 0,
                pen: Some((110.0, 700.0)),
                full_advance_ts: 10.0,
            },
            GlyphDecode {
                text: "ا".to_string(), // filler: the ligature's 2nd character
                width_ts: 0.0,
                cid: None,
                code_count: 0,
                space_count: 0,
                pen: Some((120.0, 700.0)), // shares its real glyph's post-advance position, per Phase 2
                full_advance_ts: 0.0,
            },
        ];

        let mut carry = PenContinuity::default();
        let reversed = apply_bidi_reversal(&mut glyphs, font_size, &mut carry);

        assert!(reversed);
        // Units reverse (2 units: "ت" and "لا"-ligature), but the
        // ligature's own internal order (real then filler) survives.
        assert_eq!(glyphs.len(), 3);
        assert_eq!(glyphs[0].text, "ل");
        assert_eq!(glyphs[1].text, "ا");
        assert_eq!(glyphs[2].text, "ت");
    }

    #[test]
    fn ltr_text_is_never_flagged() {
        // Plain LTR (English) text moving forward — the normal, expected
        // case for the overwhelming majority of real documents. Must
        // never be touched by this detector.
        let font_size = 10.0;
        let mut glyphs = vec![
            glyph("A", 100.0, 700.0, 10.0),
            glyph("B", 110.0, 700.0, 10.0),
            glyph("C", 120.0, 700.0, 10.0),
        ];

        let mut carry = PenContinuity::default();
        let reversed = apply_bidi_reversal(&mut glyphs, font_size, &mut carry);

        assert!(!reversed);
        let text: String = glyphs.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(text, "ABC");
    }

    #[test]
    fn single_glyph_or_zero_font_size_is_a_no_op() {
        let mut one = vec![glyph("ب", 100.0, 700.0, 10.0)];
        let mut carry = PenContinuity::default();
        assert!(!apply_bidi_reversal(&mut one, 10.0, &mut carry));

        let mut two = vec![
            glyph("ت", 100.0, 700.0, 10.0),
            glyph("ي", 110.0, 700.0, 10.0),
        ];
        let mut carry2 = PenContinuity::default();
        assert!(!apply_bidi_reversal(&mut two, 0.0, &mut carry2));
    }
}
