//! Lists MIDI ports.
fn main() {
    println!("inputs:  {:?}", deckhand_control::input_ports());
    println!("outputs: {:?}", deckhand_control::output_ports());
}
