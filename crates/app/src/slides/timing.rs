//! Click steps of a .pptx: which shapes appear at each click, read from
//! the `<p:timing>` XML inside the slide.
//!
//! A click step is what the presenter sees each time they click (or press
//! a key). Step 0 is the slide before any click; shapes with entrance
//! animations start hidden and appear at the step they belong to. The
//! effect itself (fade, fly in, wipe) is not reproduced: the shape just
//! appears, as if the effect had already finished. Exit and emphasis
//! effects are ignored (the shape stays visible once it enters).
//!
//! This is enough to present a deck whose bullets appear one by one, which
//! is the most common use of PowerPoint animations. Transitions between
//! slides (the whole-slide effect) are a separate thing, not handled here.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

/// What appears at each click of one slide.
#[derive(Clone, Debug)]
pub struct SlideSteps {
    /// The shape ids that have an entrance animation and start hidden.
    pub animated: HashSet<String>,
    /// Each click reveals these shapes (by id). Step 0 is implicit: all
    /// shapes minus `animated`. The vec has one entry per click.
    pub clicks: Vec<Vec<String>>,
}

/// A whole presentation's click steps, keyed by the slide's part path
/// (e.g. `ppt/slides/slide6.xml`).
pub type DeckSteps = HashMap<String, SlideSteps>;

/// Reads the click steps of every slide in the .pptx zip.
pub fn parse(pptx_bytes: &[u8]) -> DeckSteps {
    let Ok(archive) = zip::ZipArchive::new(Cursor::new(pptx_bytes)) else {
        return HashMap::new();
    };
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    let mut archive = match zip::ZipArchive::new(Cursor::new(pptx_bytes)) {
        Ok(a) => a,
        Err(_) => return HashMap::new(),
    };
    let mut deck = HashMap::new();
    for name in &names {
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with("ppt/slides/slide") || !lower.ends_with(".xml") {
            continue;
        }
        let Ok(mut entry) = archive.by_name(name) else {
            continue;
        };
        let mut xml = String::new();
        if entry.read_to_string(&mut xml).is_err() {
            continue;
        }
        if let Some(steps) = parse_slide(&xml) {
            deck.insert(name.clone(), steps);
        }
    }
    deck
}

/// The click steps of one slide, if it has entrance animations.
fn parse_slide(xml: &str) -> Option<SlideSteps> {
    let timing_start = xml.find("<p:timing")?;
    let timing_end = xml[timing_start..].find("</p:timing>")? + timing_start;
    let timing = &xml[timing_start..timing_end + "</p:timing>".len()];

    let main_seq_idx = timing.find("nodeType=\"mainSeq\"")?;
    let after_main = &timing[main_seq_idx..];
    let child_tn = after_main.find("<p:childTnLst")?;
    let children = &after_main[child_tn..];

    let click_groups = find_top_pars(children);

    let mut animated = HashSet::new();
    let mut clicks = Vec::new();

    for group in &click_groups {
        let mut step_shapes = Vec::new();
        let mut search_pos = 0;
        while search_pos < group.len() {
            if let Some(entr_off) = group[search_pos..].find("presetClass=\"entr\"") {
                let region_end = (search_pos + entr_off + 500).min(group.len());
                let search_region = &group[search_pos..region_end];
                for spid in find_spids(search_region) {
                    if !step_shapes.contains(&spid) {
                        step_shapes.push(spid.clone());
                    }
                    animated.insert(spid);
                }
                search_pos += entr_off + 18;
            } else {
                break;
            }
        }
        if !step_shapes.is_empty() {
            clicks.push(step_shapes);
        }
    }

    if animated.is_empty() {
        return None;
    }

    Some(SlideSteps { animated, clicks })
}

/// Finds the top-level `<p:par>` blocks inside a `<p:childTnLst>`.
fn find_top_pars(xml: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut par_depth: i32 = 0;
    let mut child_depth: i32 = 0;
    let mut par_start: Option<usize> = None;

    let start = match xml.find("<p:childTnLst") {
        Some(i) => i,
        None => return groups,
    };
    let xml = &xml[start..];

    let mut i = 0;
    while i < xml.len() {
        if xml[i..].starts_with("<p:childTnLst") {
            child_depth += 1;
            i += 14;
        } else if xml[i..].starts_with("</p:childTnLst>") {
            child_depth -= 1;
            if child_depth <= 0 {
                break;
            }
            i += 16;
        } else if xml[i..].starts_with("<p:par>") || xml[i..].starts_with("<p:par ") {
            par_depth += 1;
            // A <p:par> directly inside the first <p:childTnLst> (the one
            // at child_depth == 1) is a click group.
            if par_depth == 1 && child_depth == 1 {
                par_start = Some(i);
            }
            i += 7;
        } else if xml[i..].starts_with("</p:par>") {
            if par_depth == 1
                && child_depth == 1
                && let Some(start) = par_start.take()
            {
                groups.push(&xml[start..i + 8]);
            }
            par_depth -= 1;
            i += 8;
        } else {
            i += 1;
        }
    }
    groups
}

/// Finds all `spTgt spid="N"` values in a region of XML.
fn find_spids(xml: &str) -> Vec<String> {
    let mut spids = Vec::new();
    let needle = "spTgt spid=\"";
    let mut search_pos = 0;
    while let Some(idx) = xml[search_pos..].find(needle) {
        let start = search_pos + idx + needle.len();
        if let Some(end) = xml[start..].find('"') {
            let spid = &xml[start..start + end];
            if !spids.contains(&spid.to_owned()) {
                spids.push(spid.to_owned());
            }
            search_pos = start + end;
        } else {
            break;
        }
    }
    spids
}

/// How many images one slide with steps takes: one per step (including
/// step 0, without any clicks).
pub fn image_count(steps: &SlideSteps) -> usize {
    steps.clicks.len() + 1
}

/// Which shapes should be hidden at step `step` (0 = before any click).
/// Returns `None` if all shapes should be visible (the final step or no
/// steps at all).
pub fn hidden_at_step(steps: &SlideSteps, step: usize) -> Option<HashSet<&str>> {
    if step > steps.clicks.len() {
        return None;
    }
    let mut hidden = steps
        .animated
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    for i in 0..step {
        for spid in &steps.clicks[i] {
            hidden.remove(spid.as_str());
        }
    }
    if hidden.is_empty() {
        None
    } else {
        Some(hidden)
    }
}
