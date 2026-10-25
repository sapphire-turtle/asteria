//! `ThumbGrid` driven headlessly through `egui::Context::run`, with a counting fake source.

use asteria_framework_api::{ItemId, MediaItem};
use asteria_framework_client::images::ImageState;
use asteria_framework_client::widgets::thumb_grid::{
    SPACING, ThumbGrid, ThumbGridResponse, ThumbSource,
};
use corundum_framework::egui;
use serde_json::json;

fn item(n: usize) -> MediaItem {
    serde_json::from_value(json!({
        "id": format!("item-{n}"),
        "path": format!("{n}.jpg"),
        "kind": "image",
        "size": 1,
        "modified": "2026-01-01T00:00:00Z",
        "rating": null,
        "subjects": [],
        "hierarchical_subjects": [],
        "description": null,
        "label": null,
        "content_hash": null,
        "file_hash": null,
        "read_only_reason": null,
    }))
    .unwrap()
}

#[derive(Default)]
struct Counting {
    requested: Vec<String>,
}

impl ThumbSource for Counting {
    fn thumb(&mut self, item: &MediaItem) -> ImageState {
        self.requested.push(item.id.0.clone());
        ImageState::Loading
    }
}

/// One frame of `grid` in a `width` x `height` screen.
fn frame(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    width: f32,
    height: f32,
    grid: impl FnOnce(&mut egui::Ui) -> ThumbGridResponse,
) -> ThumbGridResponse {
    let mut out = None;
    let mut grid = Some(grid);
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, height),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            if let Some(grid) = grid.take() {
                out = Some(grid(ui));
            }
        },
    );
    // There is no renderer to hand the texture uploads to.
    output.textures_delta.clear();
    out.unwrap()
}

fn key(key: egui::Key) -> Vec<egui::Event> {
    vec![egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }]
}

#[test]
fn only_visible_rows_request_images() {
    let items: Vec<MediaItem> = (0..10_000).map(item).collect();
    let ctx = egui::Context::default();
    let mut source = Counting::default();
    let cell = 100.0;
    let response = frame(&ctx, vec![], 800.0, 600.0, |ui| {
        ThumbGrid::new(&items).cell_size(cell).show(ui, &mut source)
    });
    let rows_on_screen = (600.0 / (cell + SPACING)).ceil() as usize + 1;
    assert!(response.columns > 1);
    assert!(!source.requested.is_empty());
    assert!(
        source.requested.len() <= rows_on_screen * response.columns,
        "{} requests for at most {} cells",
        source.requested.len(),
        rows_on_screen * response.columns
    );
    // The first row is what is drawn, and nothing far below it.
    assert_eq!(source.requested[0], "item-0");
    assert!(!source.requested.contains(&"item-5000".to_string()));
}

#[test]
fn columns_follow_width() {
    let items: Vec<MediaItem> = (0..100).map(item).collect();
    let ctx = egui::Context::default();
    let mut source = Counting::default();
    let mut columns = |width: f32| {
        frame(&ctx, vec![], width, 600.0, |ui| {
            ThumbGrid::new(&items)
                .cell_size(100.0)
                .show(ui, &mut source)
        })
        .columns
    };
    let narrow = columns(300.0);
    let wide = columns(900.0);
    assert!(narrow >= 1 && wide > narrow, "{narrow} then {wide}");
    // The pure formula, with the bounds: never below one column.
    assert_eq!(ThumbGrid::columns_for(0.0, 100.0), 1);
    assert_eq!(ThumbGrid::columns_for(100.0, 100.0), 1);
    let w = 3.0 * 100.0 + 2.0 * SPACING;
    assert_eq!(ThumbGrid::columns_for(w, 100.0), 3);
    assert_eq!(ThumbGrid::columns_for(w - 0.5, 100.0), 2);
}

#[test]
fn arrow_keys_move_the_selection_by_one_and_by_a_row() {
    let items: Vec<MediaItem> = (0..100).map(item).collect();
    let ctx = egui::Context::default();
    let mut source = Counting::default();
    let mut selected: Option<ItemId> = None;
    let step = |k: egui::Key, selected: &mut Option<ItemId>, source: &mut Counting| {
        let current = selected.clone();
        let r = frame(&ctx, key(k), 800.0, 600.0, |ui| {
            ThumbGrid::new(&items)
                .cell_size(100.0)
                .selected(current)
                .show(ui, source)
        });
        if let Some(s) = r.selected.clone() {
            *selected = Some(s);
        }
        r
    };
    // Nothing is selected: an arrow selects the first item.
    step(egui::Key::ArrowRight, &mut selected, &mut source);
    assert_eq!(selected, Some(ItemId("item-0".into())));
    step(egui::Key::ArrowRight, &mut selected, &mut source);
    assert_eq!(selected, Some(ItemId("item-1".into())));
    step(egui::Key::ArrowLeft, &mut selected, &mut source);
    assert_eq!(selected, Some(ItemId("item-0".into())));
    // Left at the start stays.
    step(egui::Key::ArrowLeft, &mut selected, &mut source);
    assert_eq!(selected, Some(ItemId("item-0".into())));
    let cols = frame(&ctx, vec![], 800.0, 600.0, |ui| {
        ThumbGrid::new(&items)
            .cell_size(100.0)
            .show(ui, &mut source)
    })
    .columns;
    step(egui::Key::ArrowDown, &mut selected, &mut source);
    assert_eq!(selected, Some(ItemId(format!("item-{cols}"))));
    step(egui::Key::ArrowUp, &mut selected, &mut source);
    assert_eq!(selected, Some(ItemId("item-0".into())));
    // Enter opens the selection.
    let current = selected.clone();
    let r = frame(&ctx, key(egui::Key::Enter), 800.0, 600.0, |ui| {
        ThumbGrid::new(&items)
            .cell_size(100.0)
            .selected(current)
            .show(ui, &mut source)
    });
    assert_eq!(r.double_clicked, Some(ItemId("item-0".into())));
}
