//! Lists MIDI ports.
fn main() {
    println!("inputs:  {:?}", rofloxbox_control::input_ports());
    println!("outputs: {:?}", rofloxbox_control::output_ports());
}
