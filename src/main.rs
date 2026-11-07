//! Standalone version of the plugin, handy for development: `cargo run --release`.
use audiotomidi::AudioToMidi;
use nih_plug::prelude::*;

fn main() {
    nih_export_standalone::<AudioToMidi>();
}
