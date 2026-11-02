//! Embeds the app icon in the Windows executable (nothing elsewhere).

fn main() {
    corundum_framework_build::embed_icon("../../docs/icons/asteria-music.svg");
}
