use crate::config::{BridgeConfig, store_config};
use eframe::egui;
use eos_midi_bridge::midi::Midi;
use std::sync::{Arc, Mutex};

pub struct BridgeApp {
    pub midi: Arc<Mutex<Midi>>,
    pub config_edit: BridgeConfig,
    pub status_message: String,
    pub available_in_ports: Vec<String>,
    pub available_out_ports: Vec<String>,
}

impl BridgeApp {
    pub fn new(
        midi: Arc<Mutex<Midi>>,
        config: BridgeConfig,
        in_ports: Vec<String>,
        out_ports: Vec<String>,
    ) -> Self {
        Self {
            midi,
            config_edit: config,
            status_message: "Ready".to_string(),
            available_in_ports: in_ports,
            available_out_ports: out_ports,
        }
    }
}

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Eos Mackie Bridge Settings");
            ui.separator();

            egui::Grid::new("config_grid")
                .num_columns(2)
                .spacing([40.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Eos IP:");
                    ui.text_edit_singleline(&mut self.config_edit.eos_ip);
                    ui.end_row();

                    ui.label("Eos OSC Port:");
                    ui.add(egui::DragValue::new(&mut self.config_edit.eos_osc_port));
                    ui.end_row();

                    ui.label("Bridge Listen Port:");
                    ui.add(egui::DragValue::new(
                        &mut self.config_edit.bridge_listen_port,
                    ));
                    ui.end_row();

                    ui.label("MIDI Input:");
                    let mut selected_in = self
                        .config_edit
                        .midi_in_name
                        .clone()
                        .unwrap_or_else(|| "None".to_string());
                    egui::ComboBox::from_id_salt("midi_in_select")
                        .selected_text(&selected_in)
                        .show_ui(ui, |ui| {
                            for port in &self.available_in_ports {
                                ui.selectable_value(&mut selected_in, port.clone(), port);
                            }
                        });
                    self.config_edit.midi_in_name = Some(selected_in);
                    ui.end_row();

                    ui.label("MIDI Output:");
                    let mut selected_out = self
                        .config_edit
                        .midi_out_name
                        .clone()
                        .unwrap_or_else(|| "None".to_string());
                    egui::ComboBox::from_id_salt("midi_out_select")
                        .selected_text(&selected_out)
                        .show_ui(ui, |ui| {
                            for port in &self.available_out_ports {
                                ui.selectable_value(&mut selected_out, port.clone(), port);
                            }
                        });
                    self.config_edit.midi_out_name = Some(selected_out);
                    ui.end_row();
                });

            ui.add_space(20.0);

            if ui.button("💾 Save Configuration").clicked() {
                match store_config(&self.config_edit) {
                    Ok(_) => {
                        self.status_message = "Config saved! Restart required to apply.".to_string()
                    }
                    Err(e) => self.status_message = format!("Error: {}", e),
                }
            }

            ui.add_space(10.0);
            ui.label(&self.status_message);

            ui.separator();
            // Live Status from the Arc<Mutex<Midi>>
            if let Ok(m) = self.midi.lock() {
                ui.label(format!("Active Cue: {:.2}", m.current_cue));
                ui.label(format!("Fader Page: {}", m.fader_page));
            }
        });

        // Request constant repaints to keep the Cue display live
        ctx.request_repaint();
    }
}
