//! `asteria-music`: the Asteria Music server's command line (`init`, `api-key`, `serve`, `update`), for
//! hosts without a window.

fn main() {
    let mut spec = asteria_music_server::spec();
    spec.release_asset = Some("asteria-music-headless");
    asteria_common_server::cli::main(spec)
}
