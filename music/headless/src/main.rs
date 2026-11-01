//! `asteria-music`: the Asteria Music server's command line (`init`, `api-key`, `serve`), for
//! hosts without a window.

fn main() {
    asteria_framework_server::cli::main(asteria_music_server::spec())
}
