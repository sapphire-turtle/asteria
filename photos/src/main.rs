use asteria_photos::PhotosApp;
use corundum_framework::gui::window::{self, WindowOptions};

fn main() -> corundum_framework::eframe::Result<()> {
    window::run(WindowOptions::new("asteria-photos"), |_cc| {
        Box::new(PhotosApp::new())
    })
}
