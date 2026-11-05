//! Writes the app's icon as `icon.ico`, `icon.icns` and `icon.png` (512 px) into the
//! directory given, for the package's icons (`[package.metadata.packager]`) in the release
//! workflow. The icns is drawn from `asteria-music-macos.svg`: the mark on the brand's tile.

fn main() -> std::io::Result<()> {
    use corundum_framework::platform::icon;

    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: music-icons <dir>"));
    std::fs::create_dir_all(&dir)?;
    let svg = include_bytes!("../../../docs/icons/asteria-music.svg");
    // macOS shows the icon as it is drawn, so its own takes the brand's tile; a mark that
    // does not fill the icon grid would be set on the system's plate instead.
    let macos_svg = include_bytes!("../../../docs/icons/asteria-music-macos.svg");
    std::fs::write(dir.join("icon.ico"), icon::ico(svg)?)?;
    std::fs::write(dir.join("icon.icns"), icon::icns(macos_svg)?)?;
    std::fs::write(dir.join("icon.png"), icon::render_png(svg, 512)?)?;
    Ok(())
}
