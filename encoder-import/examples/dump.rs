//! Prints what the importer reads from a project: `cargo run -p encoder-import --example dump -- <file.fcproj>`
fn main() {
    let path = std::env::args().nth(1).expect("usage: dump <file.fcproj>");
    match encoder_import::read_project(std::path::Path::new(&path)) {
        Ok(s) => println!("{}", serde_json::to_string_pretty(&s).unwrap()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
