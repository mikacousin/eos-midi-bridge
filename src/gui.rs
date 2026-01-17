use crate::config::{BridgeConfig, store_config};
use eframe::egui;
use eos_midi_bridge::{SystemCommand, midi::Midi};
use log::{error, warn};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub struct BridgeApp {
    pub midi: Arc<Mutex<Midi>>,
    pub config_edit: BridgeConfig,
    pub last_applied_config: BridgeConfig,
    pub status_message: String,
    pub eos_ip_edit: String,
    pub tx_system: mpsc::Sender<SystemCommand>,
}

impl BridgeApp {
    pub fn new(
        midi: Arc<Mutex<Midi>>,
        config: BridgeConfig,
        tx_system: mpsc::Sender<SystemCommand>,
    ) -> Self {
        Self {
            midi,
            last_applied_config: config.clone(),
            eos_ip_edit: config.eos_ip.clone(),
            config_edit: config,
            status_message: "Ready".to_string(),
            tx_system,
        }
    }
}

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Eos Mackie Bridge Settings");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (status_text, color) = if let Ok(m) = self.midi.lock() {
                        match m.last_osc_heartbeat {
                            None => ("WAITING EOS", egui::Color32::GRAY),
                            Some(last) if last.elapsed() < std::time::Duration::from_secs(4) => {
                                ("EOS CONNECTED", egui::Color32::GREEN)
                            }
                            _ => ("EOS DISCONNECTED", egui::Color32::RED),
                        }
                    } else {
                        ("WAITING EOS", egui::Color32::GRAY)
                    };

                    ui.add_space(10.0);
                    // Draw a small circle as a LED
                    let (rect, _response) =
                        ui.allocate_at_least(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 6.0, color);
                    ui.label(
                        egui::RichText::new(status_text)
                            .strong()
                            .color(color)
                            .small(),
                    );
                });
            });
            ui.separator();

            egui::Grid::new("config_grid")
                .num_columns(2)
                .spacing([40.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Eos IP:");
                    if ui.text_edit_singleline(&mut self.eos_ip_edit).changed() {
                        self.config_edit.eos_ip = self.eos_ip_edit.clone();
                    }
                    ui.end_row();

                    ui.label("Eos OSC Port:");
                    ui.add(egui::DragValue::new(&mut self.config_edit.eos_osc_port));
                    ui.end_row();

                    ui.label("Bridge Listen Port:");
                    ui.add(egui::DragValue::new(
                        &mut self.config_edit.bridge_listen_port,
                    ));
                    ui.end_row();

                    if let Ok(m) = self.midi.lock() {
                        ui.label("MIDI Input:");
                        let mut selected_in = self.config_edit.midi_in_name.clone();

                        // Only reset to None if we actually have ports and the selected one is missing
                        if let Some(ref name) = selected_in {
                            if !m.available_in_ports.is_empty()
                                && !m.available_in_ports.contains(name)
                            {
                                selected_in = None;
                            }
                        }

                        let display_in = selected_in.as_deref().unwrap_or("None");

                        egui::ComboBox::from_id_salt("midi_in_select")
                            .selected_text(display_in)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut selected_in, None, "None");
                                for port in &m.available_in_ports {
                                    ui.selectable_value(&mut selected_in, Some(port.clone()), port);
                                }
                            });
                        self.config_edit.midi_in_name = selected_in;
                        ui.end_row();

                        ui.label("MIDI Output:");
                        let mut selected_out = self.config_edit.midi_out_name.clone();

                        // Only reset to None if we actually have ports and the selected one is missing
                        if let Some(ref name) = selected_out {
                            if !m.available_out_ports.is_empty()
                                && !m.available_out_ports.contains(name)
                            {
                                selected_out = None;
                            }
                        }

                        let display_out = selected_out.as_deref().unwrap_or("None");

                        egui::ComboBox::from_id_salt("midi_out_select")
                            .selected_text(display_out)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut selected_out, None, "None");
                                for port in &m.available_out_ports {
                                    ui.selectable_value(
                                        &mut selected_out,
                                        Some(port.clone()),
                                        port,
                                    );
                                }
                            });
                        self.config_edit.midi_out_name = selected_out;
                        ui.end_row();
                    }
                });

            ui.add_space(20.0);

            if ui.button("💾 Save to Disk").clicked() {
                match store_config(&self.config_edit) {
                    Ok(_) => {
                        self.status_message = "✓ Configuration saved to disk".to_string();
                    }
                    Err(e) => {
                        error!("Failed to save configuration: {}", e);
                        self.status_message = format!("❌ Error: {}", e);
                    }
                }
            }

            // Auto-apply changes if config has changed and is valid
            if self.config_edit != self.last_applied_config {
                if self.config_edit.validate().is_ok() {
                    if let Err(e) = self
                        .tx_system
                        .blocking_send(SystemCommand::Reconfigure(self.config_edit.clone()))
                    {
                        error!("Failed to send live reconfiguration command: {}", e);
                    } else {
                        self.last_applied_config = self.config_edit.clone();
                        self.status_message = "Settings applied".to_string();
                    }
                }
            }

            ui.separator();

            // Live Status from the Arc<Mutex<Midi>>
            match self.midi.lock() {
                Ok(m) => {
                    if let Some(cue) = m.current_cue {
                        ui.label(format!("Active Cue: {:.2}", cue));
                    } else {
                        ui.label("Active Cue: --");
                    }
                }
                Err(e) => {
                    warn!("Failed to lock MIDI state for display: {}", e);
                    ui.label("⚠ Unable to read current cue");
                }
            }

            // Fader levels
            ui.separator();
            match self.midi.lock() {
                Ok(m) => {
                    ui.heading(format!("Fader Page: {}", m.fader_page));
                    ui.add_space(5.0);

                    ui.horizontal_top(|ui| {
                        for i in 0..9 {
                            let column_width = 75.0;
                            ui.allocate_ui(egui::vec2(column_width, 200.0), |ui| {
                                ui.vertical_centered(|ui| {
                                    let name = m
                                        .fader_names
                                        .get(i)
                                        .map(|s| s.as_str())
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or("...");

                                    ui.add_sized(
                                        [column_width, 18.0],
                                        egui::Label::new(
                                            egui::RichText::new(name).small().strong(),
                                        )
                                        .truncate(),
                                    );

                                    ui.add_space(4.0);

                                    // Create a vertical slider
                                    let val = m.fader_levels.get(i).copied().unwrap_or(0.0);
                                    let mut val_mut = val;
                                    let slider = egui::Slider::new(&mut val_mut, 0.0..=1.0)
                                        .vertical()
                                        .show_value(false);

                                    // Add the slider but disable interaction
                                    ui.add_enabled_ui(false, |ui| {
                                        ui.horizontal(|ui| {
                                            let slider_width = 22.0;
                                            let padding = (column_width - slider_width) / 2.0;
                                            ui.add_space(padding);
                                            ui.add_sized([slider_width, 160.0], slider);
                                        });
                                    });

                                    ui.add_space(4.0);

                                    // Show percentage text below
                                    ui.add_sized(
                                        [column_width, 15.0],
                                        egui::Label::new(
                                            egui::RichText::new(format!("{:.0}%", val * 100.0))
                                                .small(),
                                        ),
                                    );
                                });
                            });
                        }
                    });
                }
                Err(e) => {
                    warn!("Failed to lock MIDI state for fader display: {}", e);
                    ui.label("⚠ Unable to display fader levels");
                }
            }

            // Status message en bas de la fenêtre
            ui.add_space(20.0);
            ui.separator();
            ui.horizontal(|ui| {
                if let Ok(m) = self.midi.lock() {
                    ui.label(egui::RichText::new(&m.connection_status).strong());
                    ui.label("|");
                }
                ui.label(&self.status_message);
            });
        });

        // Request repaint at a reasonable rate (30 FPS) instead of constantly
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}
