//! What the tests share: an app to set up a server for, and frames driven headlessly, with
//! clicks on what the accessibility tree names. Also used by the crate's unit tests.

// Each test binary uses its own subset of these helpers.
#![allow(dead_code)]

use std::sync::Arc;

use asteria_common_desktop::LocalServerSpec;
use asteria_common_server::AppExtension;
use asteria_common_server::cli::AppSpec;
use corundum_framework::egui;
use corundum_framework::service::ServiceSpec;

struct Nothing;

impl AppExtension for Nothing {
    fn accepts(&self, _: asteria_common_core::MediaKind) -> bool {
        false
    }
}

pub fn server() -> AppSpec {
    AppSpec {
        app: "asteria-test",
        app_api: 1,
        about: "test",
        default_listen: "127.0.0.1:7899",
        extension: Arc::new(Nothing),
        release_asset: None,
    }
}

pub fn service() -> ServiceSpec {
    asteria_common_desktop::service_spec("asteria-test", "Asteria Test server")
}

pub static SPEC: LocalServerSpec = LocalServerSpec {
    app: "asteria-test",
    server,
    service,
    url: "http://127.0.0.1:7899",
    tools: &[asteria_common_server::Tool::Vipsthumbnail],
};

/// A context with the crate's strings and the accessibility tree on.
pub fn context() -> egui::Context {
    let ctx = egui::Context::default();
    asteria_common_desktop::register(&ctx);
    ctx.enable_accesskit();
    ctx
}

/// One frame of `ui` in which `events` arrive.
pub fn frame(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    ui: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 1200.0),
            )),
            events,
            ..Default::default()
        },
        ui,
    );
    // There is no renderer to hand the texture uploads to.
    output.textures_delta.clear();
    output
}

/// Every text in the frame's accessibility tree: the labels of widgets, and the text of
/// labels (which egui gives as their value).
pub fn labels(output: &egui::FullOutput) -> Vec<String> {
    let Some(update) = &output.platform_output.accesskit_update else {
        return Vec::new();
    };
    update
        .nodes
        .iter()
        .flat_map(|(_, node)| [node.label(), node.value()])
        .flatten()
        .map(str::to_owned)
        .collect()
}

/// Whether a text in the frame contains `text`.
pub fn shows(output: &egui::FullOutput, text: &str) -> bool {
    labels(output).iter().any(|l| l.contains(text))
}

/// Where the node labelled exactly `label` is, if the frame has one.
pub fn find(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
    let update = output.platform_output.accesskit_update.as_ref()?;
    update.nodes.iter().find_map(|(_, node)| {
        let bounds = node.bounds()?;
        (node.label() == Some(label)).then(|| {
            egui::pos2(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            )
        })
    })
}

/// The events of a press at `at`, then those of its release, for two frames.
pub fn click_at(at: egui::Pos2) -> [Vec<egui::Event>; 2] {
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    [
        vec![egui::Event::PointerMoved(at), button(true)],
        vec![button(false)],
    ]
}

/// Draws `ui` until a frame shows `label`, then clicks it (two more frames). Panics if no
/// frame of the first ten shows it.
pub fn click(ctx: &egui::Context, label: &str, mut ui: impl FnMut(&mut egui::Ui)) {
    for _ in 0..10 {
        let output = frame(ctx, Vec::new(), &mut ui);
        if let Some(at) = find(&output, label) {
            for events in click_at(at) {
                frame(ctx, events, &mut ui);
            }
            return;
        }
    }
    panic!("no {label:?} to click");
}
