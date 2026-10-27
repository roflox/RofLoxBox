//! Prints what a mapping file turns MIDI messages into.
//!
//! `cargo run -p deckhand-control --example translate -- mapping.toml 94:4A:7F 84:4A:00`
//! (status:data1:data2 in hex).

use deckhand_control::{Mapping, MidiMessage, Translator};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("mapping file");
    let mut translator = Translator::new(Mapping::load(path.as_ref()).unwrap());
    for arg in args {
        let bytes: Vec<u8> = arg.split(':').map(|b| u8::from_str_radix(b, 16).unwrap()).collect();
        let message = MidiMessage::parse(&bytes).expect("unknown message");
        println!("{arg} -> {:?}", translator.translate(message));
    }
}
