//! `(model, event) → effects`, pure. Rating taps are optimistic (the stars fill this frame);
//! the runtime persists via `fm::set_scalar` and heals failures with a Rescan.

use crate::model::{Dirty, Filter, RecipeEffect, RecipeEvent, RecipeModel, Screen, Sort, Target};
use crate::view::{hit_test, rows_per_page, HitMap};

pub fn update(model: &mut RecipeModel, event: RecipeEvent, hits: &HitMap) -> Vec<RecipeEffect> {
    match event {
        RecipeEvent::Tap { x, y } => match hit_test(hits, x, y) {
            Some((_, t)) => on_tap(model, t.clone()),
            None => Vec::new(),
        },
        RecipeEvent::Rescanned(recipes) => {
            // Follow the open recipe across the re-sort by path; drop back to the library if gone.
            if let Screen::Reading { ix, .. } = &model.screen {
                let open = model.recipes.get(*ix).map(|r| r.path.clone());
                model.recipes = recipes;
                match open.and_then(|p| model.recipes.iter().position(|r| r.path == p)) {
                    Some(new_ix) => {
                        if let Screen::Reading { ix, .. } = &mut model.screen {
                            *ix = new_ix;
                        }
                    }
                    None => model.screen = Screen::Library { scroll: 0, shelf_open: false },
                }
            } else {
                model.recipes = recipes;
            }
            vec![RecipeEffect::Redraw(Dirty::Full)]
        }
        RecipeEvent::DocLoaded { ix, doc, pages } => {
            model.screen = Screen::Reading { ix, doc, pages, page_ix: 0 };
            vec![RecipeEffect::Redraw(Dirty::Full)]
        }
        RecipeEvent::Tick => {
            if let Some((_, ticks)) = &mut model.flash {
                *ticks = ticks.saturating_sub(1);
                if *ticks == 0 {
                    model.flash = None;
                    return vec![RecipeEffect::Redraw(Dirty::Full)];
                }
            }
            Vec::new()
        }
    }
}

fn on_tap(model: &mut RecipeModel, target: Target) -> Vec<RecipeEffect> {
    match target {
        Target::Open(ix) => {
            if ix < model.recipes.len() {
                return vec![RecipeEffect::LoadDoc { ix }, RecipeEffect::Redraw(Dirty::Full)];
            }
            Vec::new()
        }
        Target::Back => {
            model.screen = Screen::Library { scroll: 0, shelf_open: false };
            vec![RecipeEffect::Redraw(Dirty::Full)]
        }
        Target::PrevPage | Target::NextPage => {
            let delta: i32 = if target == Target::NextPage { 1 } else { -1 };
            if let Screen::Reading { pages, page_ix, .. } = &mut model.screen {
                let next = (*page_ix as i32 + delta).clamp(0, pages.len().saturating_sub(1) as i32) as usize;
                if next != *page_ix {
                    *page_ix = next;
                    return vec![RecipeEffect::Redraw(Dirty::Full)];
                }
            }
            Vec::new()
        }
        Target::Shelf => set_shelf(model, true),
        Target::CloseShelf => set_shelf(model, false),
        Target::Cuisine(c) => {
            model.filter.cuisine = if model.filter.cuisine.as_deref() == Some(&c) { None } else { Some(c) };
            apply_filter(model)
        }
        Target::TagChip(t) => {
            model.filter.tag = if model.filter.tag.as_deref() == Some(&t) { None } else { Some(t) };
            apply_filter(model)
        }
        Target::Quick => {
            model.filter.quick = !model.filter.quick;
            apply_filter(model)
        }
        Target::SortTitle => set_sort(model, Sort::Title),
        Target::SortTime => set_sort(model, Sort::Time),
        Target::SortRating => set_sort(model, Sort::Rating),
        Target::ClearFilters => {
            model.filter = Filter::default();
            apply_filter(model)
        }
        Target::LibPrev | Target::LibNext => {
            let per = rows_per_page();
            let n = model.visible().len();
            if let Screen::Library { scroll, .. } = &mut model.screen {
                let next = if target == Target::LibNext { *scroll + per } else { scroll.saturating_sub(per) };
                if next < n.max(1) && next != *scroll {
                    *scroll = next;
                    return vec![RecipeEffect::Redraw(Dirty::Full)];
                }
            }
            Vec::new()
        }
        Target::Rate(n) => {
            let Screen::Reading { ix, .. } = &model.screen else {
                return Vec::new();
            };
            let ix = *ix;
            let Some(r) = model.recipes.get_mut(ix) else {
                return Vec::new();
            };
            let n = n.clamp(1, 5);
            r.rating = Some(n); // optimistic — the stars fill now
            let path = r.path.clone();
            model.flash("Rated");
            vec![
                RecipeEffect::WriteRating { path, rating: n },
                RecipeEffect::Sync,
                RecipeEffect::Redraw(Dirty::Full),
            ]
        }
        Target::Refresh => {
            model.flash("Refreshing…");
            vec![RecipeEffect::Rescan, RecipeEffect::Sync, RecipeEffect::Redraw(Dirty::Full)]
        }
        Target::Exit => vec![RecipeEffect::Exit],
    }
}

fn set_shelf(model: &mut RecipeModel, open: bool) -> Vec<RecipeEffect> {
    if let Screen::Library { shelf_open, .. } = &mut model.screen {
        if *shelf_open != open {
            *shelf_open = open;
            return vec![RecipeEffect::Redraw(Dirty::Full)];
        }
    }
    Vec::new()
}

/// A filter change re-bases the list: back to the top, shelf stays open for more tweaks.
fn apply_filter(model: &mut RecipeModel) -> Vec<RecipeEffect> {
    if let Screen::Library { scroll, .. } = &mut model.screen {
        *scroll = 0;
    }
    vec![RecipeEffect::Redraw(Dirty::Full)]
}

fn set_sort(model: &mut RecipeModel, sort: Sort) -> Vec<RecipeEffect> {
    model.sort = sort;
    apply_filter(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::RecipeFile;
    use std::path::PathBuf;

    fn recipe(title: &str, cuisine: Option<&str>, time: Option<u32>, tags: &[&str], rating: Option<u8>) -> RecipeFile {
        RecipeFile {
            path: PathBuf::from(format!("/r/{title}.md")),
            title_lower: title.to_lowercase(),
            title: title.into(),
            cuisine: cuisine.map(String::from),
            time_min: time,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            rating,
        }
    }
    fn sample() -> RecipeModel {
        RecipeModel::new(vec![
            recipe("Carbonara", Some("Italian"), Some(25), &["dinner"], Some(5)),
            recipe("Katsu curry", Some("Japanese"), Some(50), &["dinner"], Some(3)),
            recipe("Shakshuka", Some("Middle Eastern"), Some(30), &["breakfast", "eggs"], None),
        ])
    }

    #[test]
    fn filters_and_sorts() {
        let mut m = sample();
        assert_eq!(m.visible(), [0, 1, 2], "title order by default");
        m.filter.quick = true;
        assert_eq!(m.visible(), [0, 2], "quick keeps ≤30 min only");
        m.filter = Filter { tag: Some("dinner".into()), ..Default::default() };
        assert_eq!(m.visible(), [0, 1]);
        m.filter = Filter { cuisine: Some("Japanese".into()), ..Default::default() };
        assert_eq!(m.visible(), [1]);
        m.filter = Filter::default();
        m.sort = Sort::Time;
        assert_eq!(m.visible(), [0, 2, 1]);
        m.sort = Sort::Rating;
        assert_eq!(m.visible(), [0, 1, 2], "rating desc, unrated last");
    }

    #[test]
    fn chip_taps_toggle_and_rebase_scroll() {
        let mut m = sample();
        if let Screen::Library { scroll, .. } = &mut m.screen {
            *scroll = 2;
        }
        on_tap(&mut m, Target::Cuisine("Italian".into()));
        assert_eq!(m.filter.cuisine.as_deref(), Some("Italian"));
        assert!(matches!(m.screen, Screen::Library { scroll: 0, .. }), "filter change returns to the top");
        on_tap(&mut m, Target::Cuisine("Italian".into()));
        assert_eq!(m.filter.cuisine, None, "re-tap clears");
    }

    #[test]
    fn rating_is_optimistic_and_enqueues_the_write() {
        let mut m = sample();
        m.screen = Screen::Reading { ix: 2, doc: reader::parse(""), pages: vec![], page_ix: 0 };
        let eff = on_tap(&mut m, Target::Rate(4));
        assert_eq!(m.recipes[2].rating, Some(4), "stars fill immediately");
        assert!(matches!(&eff[0], RecipeEffect::WriteRating { rating: 4, path } if path.ends_with("Shakshuka.md")));
        assert!(eff.contains(&RecipeEffect::Sync));
    }

    #[test]
    fn opening_a_recipe_round_trips_through_the_runtime() {
        let mut m = sample();
        let eff = on_tap(&mut m, Target::Open(1));
        assert!(matches!(&eff[0], RecipeEffect::LoadDoc { ix: 1 }));
        let doc = reader::parse("# Katsu\n\nSteps.");
        let pages = reader::layout(&doc, &render::Fonts::embedded(), reader::LayoutOpts::default());
        update(&mut m, RecipeEvent::DocLoaded { ix: 1, doc, pages }, &Vec::new());
        assert!(matches!(m.screen, Screen::Reading { ix: 1, page_ix: 0, .. }));
    }
}
