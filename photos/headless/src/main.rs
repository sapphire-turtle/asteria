//! `asteria-photos`: the Asteria Photos server's command line (`init`, `api-key`, `serve`), for
//! hosts without a window.

fn main() {
    asteria_framework_server::cli::main(asteria_photos_server::spec())
}
