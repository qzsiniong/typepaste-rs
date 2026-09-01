mod backend;
mod cli;
mod config;
mod encoder;
mod failsafe;
mod keymap;
mod ocr;
mod pull;
mod restore_script;
mod utils;

fn main() {
    cli::main();
}
