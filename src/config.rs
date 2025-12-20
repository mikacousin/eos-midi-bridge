// Existing imports and above code remain unchanged

impl Config {
    pub fn default() -> Self {
        Self {
            // ... other fields ...
            mappings: vec![
                // ... existing mappings ...
                MidiOscMapping {
                    event_type: MidiEventType::NoteOn,
                    data_number: 94,
                    osc_address: "/eos/key/go".to_string(),
                    fixed_osc_value: Some(1.0),
                    ..Default::default()
                },
            ],
        }
    }
}
// Rest of src/config.rs remains unchanged