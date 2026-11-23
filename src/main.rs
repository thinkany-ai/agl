fn main() {
    // A product launcher (`__hold`) or the computer-use helper stays a small
    // process: no runtime, no CLI.
    if let Some(code) = agl::app::hold_from_args() {
        std::process::exit(code);
    }
    std::process::exit(agl::cli::run());
}
