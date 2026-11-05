//! `asteria-photos`: the Asteria Photos server's command line (`init`, `api-key`, `serve`, `update`), for
//! hosts without a window.

fn main() {
    let mut spec = asteria_photos_server::spec();
    spec.release_asset = Some("asteria-photos-headless");
    asteria_common_server::cli::main(spec)
}
