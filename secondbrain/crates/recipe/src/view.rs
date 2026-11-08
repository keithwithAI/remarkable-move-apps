//! Draw the browser's two screens to a frame + a hit map. Reading Desk language throughout;
//! recipe pages render through the reader crate's pure `draw_page`.

use crate::model::{RecipeModel, Screen, Sort, Target};
use image::RgbImage;
use render::draw::{draw_text, fill_rect, hline, icon, measure, truncate};
use render::ui::{chip, even_slots, icon_btn, panel_eyebrow, draw_panel, Icon, Rect, GOLD, PANEL_W};
use render::{Canvas, Fonts, FAINT, INK, MUTED, PAPER};

pub type HitMap = Vec<(Rect, Target)>;

/// Topmost hit wins — the map is searched in reverse draw order.
pub fn hit_test(hits: &HitMap, x: i32, y: i32) -> Option<&(Rect, Target)> {
    hits.iter().rev().find(|(r, _)| r.contains(x, y))
}

pub struct ViewOutput {
    pub frame: RgbImage,
    pub hits: HitMap,
}

const MARGIN: i32 = 56;
const ROW_H: i32 = 148;
const BAR_H: i32 = 108;
const LIST_TOP: i32 = 208;
/// The reading view's layout: body above a reserved rating row, page-1 masthead space.
pub const READ_TOP: f32 = 88.0;
pub const READ_BOTTOM: f32 = 1470.0;
pub const READ_FIRST_EXTRA: f32 = 120.0;
const RATING_Y: i32 = 1500;

/// Library rows per page (shared with update's paging).
pub fn rows_per_page() -> usize {
    ((1696 - LIST_TOP - BAR_H) / ROW_H) as usize
}

pub fn view(model: &RecipeModel, fonts: &Fonts, canvas: Canvas) -> ViewOutput {
    let mut img = RgbImage::from_pixel(canvas.width, canvas.height, PAPER);
    let mut hits: HitMap = Vec::new();
    let (w, h) = (canvas.width as i32, canvas.height as i32);

    match &model.screen {
        Screen::Library { scroll, shelf_open } => {
            draw_library(&mut img, &mut hits, model, fonts, w, h, *scroll);
            draw_bottom_bar(&mut img, &mut hits, model, fonts, w, h);
            if *shelf_open {
                draw_shelf(&mut img, &mut hits, model, fonts, w, h);
            }
        }
        Screen::Reading { ix, pages, page_ix, .. } => {
            draw_reading(&mut img, &mut hits, model, fonts, w, h, *ix, pages, *page_ix);
        }
    }

    ViewOutput { frame: img, hits }
}

fn draw_library(img: &mut RgbImage, hits: &mut HitMap, model: &RecipeModel, fonts: &Fonts, w: i32, h: i32, scroll: usize) {
    draw_text(img, &fonts.serif_bold, 64.0, MARGIN as f32, 108.0, "Recipes", INK);
    let visible = model.visible();
    let mut line = format!("{} RECIPE{}", visible.len(), if visible.len() == 1 { "" } else { "S" });
    if model.filter.cuisine.is_some() || model.filter.tag.is_some() || model.filter.quick {
        line.push_str(" · FILTERED");
    }
    draw_text(img, &fonts.mono, 28.0, MARGIN as f32, 152.0, &line, MUTED);
    hits.push((icon_btn(img, fonts, Icon::Menu, w - MARGIN - 84, 60, 84, 84, INK), Target::Shelf));
    hline(img, MARGIN as f32, (w - MARGIN) as f32, 176.0, FAINT, 1);

    if model.recipes.is_empty() {
        return draw_empty_state(img, fonts, w);
    }

    let per = rows_per_page();
    let mut y = LIST_TOP;
    for &ix in visible.iter().skip(scroll).take(per) {
        let r = &model.recipes[ix];
        let base = (y + 62) as f32;
        let title = truncate(&fonts.sans_semibold, 40.0, &r.title, (w - 2 * MARGIN) as f32);
        draw_text(img, &fonts.sans_semibold, 40.0, MARGIN as f32, base, &title, INK);
        // The metadata line: cuisine · time · stars.
        let mut meta = Vec::new();
        if let Some(c) = &r.cuisine {
            meta.push(c.clone());
        }
        if let Some(t) = r.time_min {
            meta.push(format!("{t} min"));
        }
        let meta = meta.join(" · ");
        let mx = draw_text(img, &fonts.mono, 28.0, MARGIN as f32, base + 44.0, &meta, MUTED);
        if let Some(n) = r.rating {
            draw_stars(img, fonts, MARGIN as f32 + mx + if meta.is_empty() { 0.0 } else { 24.0 }, base + 20.0, n, 30);
        }
        hits.push((Rect::new(0.0, y as f32, w as f32, ROW_H as f32), Target::Open(ix)));
        y += ROW_H;
        hline(img, MARGIN as f32, (w - MARGIN) as f32, (y - 6) as f32, FAINT, 1);
    }
    // Page count + paging when the list overflows.
    if visible.len() > per {
        let label = format!("{}–{} of {}", scroll + 1, (scroll + per).min(visible.len()), visible.len());
        let lw = measure(&fonts.mono, 26.0, &label);
        draw_text(img, &fonts.mono, 26.0, (w as f32 - lw) / 2.0, (h - BAR_H - 20) as f32, &label, MUTED);
        if scroll > 0 {
            hits.push((icon_btn(img, fonts, Icon::ChevronLeft, MARGIN, h - BAR_H - 64, 84, 60, INK), Target::LibPrev));
        }
        if scroll + per < visible.len() {
            hits.push((icon_btn(img, fonts, Icon::ChevronRight, w - MARGIN - 84, h - BAR_H - 64, 84, 60, INK), Target::LibNext));
        }
    }
}

/// Five small stars starting at `(x, y)` (top-left), `sz` px each; `n` filled.
fn draw_stars(img: &mut RgbImage, fonts: &Fonts, x: f32, y: f32, n: u8, sz: i32) {
    for i in 0..5u8 {
        let ic = if i < n { Icon::StarFill } else { Icon::Star };
        let ink = if i < n { GOLD } else { FAINT };
        icon(img, &fonts.icons, ic.ch(), x + i as f32 * (sz as f32 + 8.0) + sz as f32 / 2.0, y + sz as f32 / 2.0, sz as f32, ink);
    }
}

fn draw_shelf(img: &mut RgbImage, hits: &mut HitMap, model: &RecipeModel, fonts: &Fonts, w: i32, h: i32) {
    let (panel, close) = draw_panel(img, fonts, "Filter & sort", w, h);
    // A tap outside the panel closes it; the panel body swallows.
    hits.push((Rect::new(0.0, 0.0, w as f32, h as f32), Target::CloseShelf));
    hits.push((panel, Target::Shelf)); // swallow (re-tapping the shelf is a no-op open)
    hits.push((close, Target::CloseShelf));

    let pad = 40;
    let x = pad;
    let chip_h = 72;
    let mut y = 190;

    panel_eyebrow(img, fonts, "SORT", x, y);
    y += 24;
    for (label, target, active) in [
        ("Title", Target::SortTitle, model.sort == Sort::Title),
        ("Time", Target::SortTime, model.sort == Sort::Time),
        ("Rating", Target::SortRating, model.sort == Sort::Rating),
    ] {
        let cw = measure(&fonts.sans, 30.0, label) as i32 + 56;
        hits.push((chip(img, fonts, label, x, y, cw, chip_h, active), target));
        y += chip_h + 16;
    }

    y += 24;
    panel_eyebrow(img, fonts, "QUICK", x, y);
    y += 24;
    let cw = measure(&fonts.sans, 30.0, "≤ 30 min") as i32 + 56;
    hits.push((chip(img, fonts, "≤ 30 min", x, y, cw, chip_h, model.filter.quick), Target::Quick));
    y += chip_h + 40;

    y = chip_section(img, hits, fonts, "CUISINE", &model.cuisines(), model.filter.cuisine.as_deref(), x, y, chip_h, |c| Target::Cuisine(c));
    y = chip_section(img, hits, fonts, "TAGS", &model.all_tags(), model.filter.tag.as_deref(), x, y, chip_h, |t| Target::TagChip(t));

    if model.filter.cuisine.is_some() || model.filter.tag.is_some() || model.filter.quick {
        let cw = measure(&fonts.sans, 30.0, "Clear filters") as i32 + 56;
        hits.push((chip(img, fonts, "Clear filters", x, y + 16, cw, chip_h, false), Target::ClearFilters));
    }
}

/// An eyebrow + a wrapped row of chips; returns the next free y.
#[allow(clippy::too_many_arguments)]
fn chip_section(
    img: &mut RgbImage,
    hits: &mut HitMap,
    fonts: &Fonts,
    eyebrow: &str,
    options: &[String],
    active: Option<&str>,
    x: i32,
    mut y: i32,
    chip_h: i32,
    target: impl Fn(String) -> Target,
) -> i32 {
    if options.is_empty() {
        return y;
    }
    panel_eyebrow(img, fonts, eyebrow, x, y);
    y += 24;
    let mut cx = x;
    for opt in options {
        let cw = measure(&fonts.sans, 30.0, opt) as i32 + 56;
        if cx + cw > PANEL_W - 32 {
            cx = x;
            y += chip_h + 14;
        }
        hits.push((chip(img, fonts, opt, cx, y, cw, chip_h, active == Some(opt.as_str())), target(opt.clone())));
        cx += cw + 14;
    }
    y + chip_h + 40
}

#[allow(clippy::too_many_arguments)]
fn draw_reading(img: &mut RgbImage, hits: &mut HitMap, model: &RecipeModel, fonts: &Fonts, w: i32, h: i32, ix: usize, pages: &[reader::Page], page_ix: usize) {
    let Some(r) = model.recipes.get(ix) else {
        return;
    };
    // The recipe page body (laid out by the runtime with READ_* opts).
    if let Some(page) = pages.get(page_ix) {
        let body = reader::draw_page(page, fonts, w as u32, h as u32);
        *img = body;
    }
    // Page-1 masthead in the reserved band: serif title + cuisine·time byline.
    if page_ix == 0 {
        draw_text(img, &fonts.serif_bold, 52.0, MARGIN as f32, 148.0, &truncate(&fonts.serif_bold, 52.0, &r.title, (w - 2 * MARGIN) as f32), INK);
        let mut by = Vec::new();
        if let Some(c) = &r.cuisine {
            by.push(c.clone());
        }
        if let Some(t) = r.time_min {
            by.push(format!("{t} min"));
        }
        if !by.is_empty() {
            draw_text(img, &fonts.sans_semibold, 26.0, MARGIN as f32, 190.0, &by.join(" · ").to_uppercase(), MUTED);
        }
    }
    // Slim top chrome: back + page N/M.
    hits.push((icon_btn(img, fonts, Icon::ChevronLeft, MARGIN, 8, 72, 68, INK), Target::Back));
    let label = format!("{}/{}", page_ix + 1, pages.len().max(1));
    let lw = measure(&fonts.mono, 26.0, &label);
    draw_text(img, &fonts.mono, 26.0, (w - MARGIN) as f32 - lw, 52.0, &label, MUTED);

    // Rating row: five stars, each its own generous cell.
    hline(img, MARGIN as f32, (w - MARGIN) as f32, RATING_Y as f32 - 24.0, FAINT, 1);
    draw_text(img, &fonts.sans_semibold, 26.0, MARGIN as f32, RATING_Y as f32 + 52.0, "RATE", MUTED);
    let cell = 96;
    let slots = even_slots(w - 220, MARGIN + 180, cell, 5);
    let rating = r.rating.unwrap_or(0);
    for (i, sx) in slots.iter().enumerate() {
        let n = i as u8 + 1;
        let ic = if n <= rating { Icon::StarFill } else { Icon::Star };
        let ink = if n <= rating { GOLD } else { INK };
        hits.push((icon_btn(img, fonts, ic, sx + 180, RATING_Y, cell, 96, ink), Target::Rate(n)));
    }

    // Side-third page turns over the body (pushed first-drawn = lowest priority; chrome + stars
    // were pushed above, but hit_test is reverse-draw so push these BEFORE… they already are not.
    // Simplest: push now with narrow bands that avoid the chrome/rating rows.
    let third = w / 3;
    let body_top = 80;
    let body_bot = RATING_Y - 32;
    if page_ix > 0 {
        hits.insert(0, (Rect::new(0.0, body_top as f32, third as f32, (body_bot - body_top) as f32), Target::PrevPage));
    }
    if page_ix + 1 < pages.len() {
        hits.insert(0, (Rect::new((w - third) as f32, body_top as f32, third as f32, (body_bot - body_top) as f32), Target::NextPage));
    }
}

fn draw_empty_state(img: &mut RgbImage, fonts: &Fonts, w: i32) {
    let x = MARGIN as f32;
    draw_text(img, &fonts.sans, 34.0, x, 320.0, "No recipe notes yet.", INK);
    draw_text(img, &fonts.sans, 30.0, x, 380.0, "Create notes in Obsidian under Clippings/Recipes/ like:", MUTED);
    let template = [
        "---",
        "type: recipe",
        "cuisine: Italian",
        "time_min: 25",
        "tags: [dinner, pasta]",
        "rating: 5",
        "---",
        "## Ingredients",
        "## Steps",
    ];
    let mut y = 452.0;
    fill_rect(img, x - 16.0, y - 40.0, (w - 2 * MARGIN + 32) as f32, 6.0 + 48.0 * template.len() as f32, render::ui::CHIP);
    for line in template {
        draw_text(img, &fonts.mono, 28.0, x, y, line, INK);
        y += 48.0;
    }
    draw_text(img, &fonts.sans, 30.0, x, y + 40.0, "They sync to the device within a few minutes.", MUTED);
}

fn draw_bottom_bar(img: &mut RgbImage, hits: &mut HitMap, model: &RecipeModel, fonts: &Fonts, w: i32, h: i32) {
    let bar_y = h - BAR_H;
    hline(img, MARGIN as f32, (w - MARGIN) as f32, bar_y as f32, FAINT, 2);
    if let Some((text, _)) = &model.flash {
        let fw = measure(&fonts.mono, 26.0, text);
        draw_text(img, &fonts.mono, 26.0, (w as f32 - fw) / 2.0, (bar_y - 14) as f32, text, MUTED);
    }
    let icell = 84;
    let slots = even_slots(w, MARGIN, icell, 2);
    let icon_y = bar_y + 14;
    hits.push((icon_btn(img, fonts, Icon::Undo, slots[0], icon_y, icell, 72, INK), Target::Refresh));
    hits.push((icon_btn(img, fonts, Icon::Exit, slots[1], icon_y, icell, 72, INK), Target::Exit));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::RecipeFile;
    use crate::model::RecipeModel;
    use std::path::PathBuf;

    const CANVAS: Canvas = Canvas { width: 954, height: 1696 };

    fn recipe(title: &str) -> RecipeFile {
        RecipeFile {
            path: PathBuf::from(format!("/r/{title}.md")),
            title_lower: title.to_lowercase(),
            title: title.into(),
            cuisine: Some("Italian".into()),
            time_min: Some(25),
            tags: vec!["dinner".into()],
            rating: Some(4),
        }
    }

    #[test]
    fn library_rows_open_and_shelf_filters_are_tappable() {
        let mut model = RecipeModel::new(vec![recipe("Carbonara"), recipe("Ragu")]);
        let out = view(&model, &Fonts::embedded(), CANVAS);
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Open(0)));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Shelf));
        // Open the shelf: sort + quick + cuisine + tag chips all present.
        if let Screen::Library { shelf_open, .. } = &mut model.screen {
            *shelf_open = true;
        }
        let out = view(&model, &Fonts::embedded(), CANVAS);
        for t in [Target::SortTitle, Target::SortTime, Target::SortRating, Target::Quick, Target::Cuisine("Italian".into()), Target::TagChip("dinner".into()), Target::CloseShelf] {
            assert!(out.hits.iter().any(|(_, tt)| *tt == t), "{t:?} present");
        }
    }

    #[test]
    fn reading_view_has_thirds_stars_and_back() {
        let mut model = RecipeModel::new(vec![recipe("Carbonara")]);
        let doc = reader::parse(&("Step. ".repeat(600)));
        let opts = reader::LayoutOpts { content_top: READ_TOP, content_bottom: READ_BOTTOM, first_page_extra: READ_FIRST_EXTRA, ..Default::default() };
        let pages = reader::layout(&doc, &Fonts::embedded(), opts);
        assert!(pages.len() > 1);
        model.screen = Screen::Reading { ix: 0, doc, pages, page_ix: 0 };
        let out = view(&model, &Fonts::embedded(), CANVAS);
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Back));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::NextPage), "page 1 of many has next");
        assert!(!out.hits.iter().any(|(_, t)| *t == Target::PrevPage), "no prev on page 1");
        for n in 1..=5u8 {
            assert!(out.hits.iter().any(|(_, t)| *t == Target::Rate(n)), "star {n}");
        }
        // Stars sit above the body thirds in hit priority (they were pushed later).
        let star = out.hits.iter().rev().find(|(_, t)| *t == Target::Rate(5)).unwrap();
        let (cx, cy) = (star.0.x + star.0.w / 2, star.0.y + star.0.h / 2);
        assert_eq!(hit_test(&out.hits, cx, cy).map(|(_, t)| t), Some(&Target::Rate(5)), "star wins over the page-turn third");
    }

    #[test]
    fn empty_dir_renders_the_template() {
        let model = RecipeModel::new(vec![]);
        let out = view(&model, &Fonts::embedded(), CANVAS);
        let ink = out.frame.pixels().filter(|p| (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384).count();
        assert!(ink > 1500, "template drawn");
    }
}
