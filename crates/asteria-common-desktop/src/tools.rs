//! Whether the server's external tools are on this computer, and how to get them when not.
//! The server looks for them with the same function, so what this shows is what it finds.

use std::path::PathBuf;

use asteria_common_server::{ImageTool, Tool, find_image_tool, find_tool};
use corundum_framework::egui;
use corundum_framework::gui::tr;

/// Where each tool was found, as last looked up: kept in the context's memory, so a frame
/// does not search `PATH` again.
#[derive(Clone, Default)]
struct Found {
    paths: Vec<Option<PathBuf>>,
    /// Whether images are made with macOS's `sips` (no `vipsthumbnail`).
    sips: bool,
}

fn found_id(tools: &[Tool]) -> egui::Id {
    egui::Id::unique(("asteria_desktop_tools_found", tools))
}

fn look_up(tools: &[Tool]) -> Found {
    Found {
        paths: tools.iter().map(|&tool| find_tool(tool)).collect(),
        sips: matches!(find_image_tool(), Some(ImageTool::Sips(_))),
    }
}

/// The program's name, the variable naming another path, and the message saying what the
/// server uses it for.
fn about(tool: Tool) -> (&'static str, &'static str, &'static str) {
    match tool {
        Tool::Vipsthumbnail => (
            "vipsthumbnail",
            "ASTERIA_VIPSTHUMBNAIL",
            "asteria-common-desktop-tools-vips-purpose",
        ),
        Tool::Ffmpeg => (
            "ffmpeg",
            "ASTERIA_FFMPEG",
            "asteria-common-desktop-tools-ffmpeg-purpose",
        ),
    }
}

/// How to install `tool` on this system.
fn install_hint(ui: &egui::Ui, tool: Tool) -> String {
    if cfg!(target_os = "windows") {
        match tool {
            Tool::Vipsthumbnail => tr!(ui, "asteria-common-desktop-tools-windows-vips"),
            Tool::Ffmpeg => tr!(ui, "asteria-common-desktop-tools-windows-ffmpeg"),
        }
    } else if cfg!(target_os = "macos") {
        let package = match tool {
            Tool::Vipsthumbnail => "vips",
            Tool::Ffmpeg => "ffmpeg",
        };
        tr!(ui, "asteria-common-desktop-tools-macos", package = package)
    } else {
        let package = match tool {
            Tool::Vipsthumbnail => "libvips-tools",
            Tool::Ffmpeg => "ffmpeg",
        };
        tr!(ui, "asteria-common-desktop-tools-linux", package = package)
    }
}

/// The app's external tools (its [`LocalServerSpec::tools`](crate::LocalServerSpec)): for
/// each, where it is, or that it is missing and how to install it. Without them the server
/// still runs; it only lacks the thumbnails these programs add. Shared by the first run and the settings page.
pub fn tools_ui(ui: &mut egui::Ui, tools: &[Tool]) {
    if tools.is_empty() {
        ui.label(tr!(ui, "asteria-common-desktop-tools-none"));
        return;
    }
    let ctx = ui.ctx().clone();
    let id = found_id(tools);
    let found = match ctx.data(|d| d.get_temp::<Found>(id)) {
        Some(found) => found,
        None => {
            let found = look_up(tools);
            ctx.data_mut(|d| d.insert_temp(id, found.clone()));
            found
        }
    };
    ui.label(tr!(ui, "asteria-common-desktop-tools-intro"));
    ui.add_space(8.0);
    for (&tool, path) in tools.iter().zip(&found.paths) {
        let (name, var, purpose) = about(tool);
        ui.strong(name);
        ui.label(tr!(ui, purpose));
        match path {
            Some(path) => {
                ui.label(tr!(
                    ui,
                    "asteria-common-desktop-tools-found",
                    path = path.display().to_string()
                ));
            }
            None if tool == Tool::Vipsthumbnail && found.sips => {
                ui.label(tr!(ui, "asteria-common-desktop-tools-sips"));
            }
            None => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    tr!(ui, "asteria-common-desktop-tools-missing"),
                );
                ui.label(install_hint(ui, tool));
                ui.label(tr!(ui, "asteria-common-desktop-tools-env", var = var));
            }
        }
        ui.add_space(8.0);
    }
    if ui
        .button(tr!(ui, "asteria-common-desktop-tools-check-again"))
        .clicked()
    {
        ctx.data_mut(|d| d.insert_temp(id, look_up(tools)));
    }
}
