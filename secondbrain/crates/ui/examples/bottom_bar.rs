//! Render the Agenda's bottom bar and the task action card, so the six-cell layout and the card's
//! two shapes can be eyeballed without a device.
//!
//!   cargo run -p ui --example bottom_bar -- /tmp/bars
//!
//! Writes bar-plain.png / bar-flash.png (the bar alone), plus card-full.png (defer rows shown) and
//! card-nodefer.png (a recurring task, where deferring would break the recurrence). The card used
//! to have a third shape for a block whose event carried no Diarized action link; Recalculate is
//! gone, so that distinction no longer exists.

use chrono::{Local, TimeZone};
use render::{Canvas, Fonts};
use sb_core::PlannerState;
use ui::model::Overlay;
use ui::view::BOTTOM_BAR_H;
use ui::{AppModel, Flash};

const CANVAS: Canvas = Canvas { width: 954, height: 1696 };

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&out).expect("create out dir");
    let fonts = Fonts::embedded();
    let now = Local.with_ymd_and_hms(2026, 7, 3, 9, 30, 0).unwrap();
    let day = now.date_naive();
    let planner = PlannerState::build(vec![], vec![], (day, day), day, now);

    let card = |can_defer: bool| Overlay::TaskCard {
        task_id: "t1".into(),
        title: "Draft the integration notes".into(),
        can_defer,
        priority: 4,
    };

    // The bar on its own — cropped, so only the six cells are under review.
    for (name, setup) in [
        ("plain", (|_: &mut AppModel| {}) as fn(&mut AppModel)),
        ("flash", |m: &mut AppModel| {
            m.flash = Some(Flash { text: "Completed".into(), expires_at: m.now });
        }),
    ] {
        let mut model = AppModel::new(planner.clone(), now);
        setup(&mut model);
        let frame = ui::view(&model, &fonts, CANVAS, now).frame;
        let bar = image::imageops::crop_imm(
            &frame,
            0,
            CANVAS.height - BOTTOM_BAR_H as u32,
            CANVAS.width,
            BOTTOM_BAR_H as u32,
        )
        .to_image();
        let path = format!("{out}/bar-{name}.png");
        bar.save(&path).expect("save png");
        println!("wrote {path}");
    }

    // The full frame for each card shape — the sheet grows/shrinks with the rows it shows.
    for (name, overlay) in [("full", card(true)), ("nodefer", card(false))] {
        let mut model = AppModel::new(planner.clone(), now);
        model.overlay = Some(overlay);
        let frame = ui::view(&model, &fonts, CANVAS, now).frame;
        let path = format!("{out}/card-{name}.png");
        frame.save(&path).expect("save png");
        println!("wrote {path}");
    }
}
