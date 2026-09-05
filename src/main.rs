mod backend;
mod cli;
mod config;
mod encoder;
mod failsafe;
mod keymap;
mod ocr;
mod pull;
mod qr;
mod restore_script;
mod utils;
mod web;

fn main() {
    cli::main();
}
