//! Whether the server's external tools are on this computer, and how to get them when not.
//! The server looks for them with the same function, so what this shows is what it finds.

use std::path::PathBuf;

use asteria_framework_server::{Tool, find_tool};
use corundum_framework::egui;
use corundum_framework::gui::tr;

/// Where each tool was found, as last looked up: kept in the context's memory, so a frame
/// does not search `PATH` again.
#[derive(Clone, Default)]
struct Found(Vec<Option<PathBuf>>);

const TOOLS: [Tool; 2] = [Tool::Vipsthumbnail, Tool::Ffmpeg];

fn found_id() -> egui::Id {
    egui::Id::unique("asteria_desktop_tools_found")
}

fn look_up() -> Found {
    Found(TOOLS.iter().map(|&tool| find_tool(tool)).collect())
}

/// The program's name, the variable naming another path, and the message saying what the
/// server uses it for.
fn about(tool: Tool) -> (&'static str, &'static str, &'static str) {
    match tool {
        Tool::Vipsthumbnail => (
            "vipsthumbnail",
            "ASTERIA_VIPSTHUMBNAIL",
            "asteria-desktop-tools-vips-purpose",
        ),
        Tool::Ffmpeg => (
            "ffmpeg",
            "ASTERIA_FFMPEG",
            "asteria-desktop-tools-ffmpeg-purpose",
        ),
    }
}

/// How to install `tool` on this system.
fn install_hint(ui: &egui::Ui, tool: Tool) -> String {
    if cfg!(target_os = "windows") {
        match tool {
            Tool::Vipsthumbnail => tr!(ui, "asteria-desktop-tools-windows-vips"),
            Tool::Ffmpeg => tr!(ui, "asteria-desktop-tools-windows-ffmpeg"),
        }
    } else if cfg!(target_os = "macos") {
        let package = match tool {
            Tool::Vipsthumbnail => "vips",
            Tool::Ffmpeg => "ffmpeg",
        };
        tr!(ui, "asteria-desktop-tools-macos", package = package)
    } else {
        let package = match tool {
            Tool::Vipsthumbnail => "libvips-tools",
            Tool::Ffmpeg => "ffmpeg",
        };
        tr!(ui, "asteria-desktop-tools-linux", package = package)
    }
}

/// The external tools: for each, where it is, or that it is missing and how to install it.
/// Without them the server still runs; it only makes no thumbnails or smaller copies. Shared
/// by the first run and the settings page.
pub fn tools_ui(ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let found = match ctx.data(|d| d.get_temp::<Found>(found_id())) {
        Some(found) => found,
        None => {
            let found = look_up();
            ctx.data_mut(|d| d.insert_temp(found_id(), found.clone()));
            found
        }
    };
    ui.label(tr!(ui, "asteria-desktop-tools-intro"));
    ui.add_space(8.0);
    for (&tool, path) in TOOLS.iter().zip(&found.0) {
        let (name, var, purpose) = about(tool);
        ui.strong(name);
        ui.label(tr!(ui, purpose));
        match path {
            Some(path) => {
                ui.label(tr!(
                    ui,
                    "asteria-desktop-tools-found",
                    path = path.display().to_string()
                ));
            }
            None => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    tr!(ui, "asteria-desktop-tools-missing"),
                );
                ui.label(install_hint(ui, tool));
                ui.label(tr!(ui, "asteria-desktop-tools-env", var = var));
            }
        }
        ui.add_space(8.0);
    }
    if ui
        .button(tr!(ui, "asteria-desktop-tools-check-again"))
        .clicked()
    {
        ctx.data_mut(|d| d.insert_temp(found_id(), look_up()));
    }
}
