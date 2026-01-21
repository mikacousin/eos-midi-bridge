use crate::config::{BridgeConfig, store_config};
use eframe::egui;
use eos_midi_bridge::{SystemCommand, midi::Midi};
use log::{error, warn};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    NodeGraph,
    Console,
    Settings,
}

pub struct BridgeApp {
    pub midi: Arc<Mutex<Midi>>,
    pub config_edit: BridgeConfig,
    pub last_applied_config: BridgeConfig,
    pub status_message: String,
    pub eos_ip_edit: String,
    pub controller_profile_edit: String,
    pub tx_system: mpsc::Sender<SystemCommand>,
    pub file_dialog: egui_file_dialog::FileDialog,
    active_tab: Tab,
    pub snarl: egui_snarl::Snarl<eos_midi_bridge::nodes::NodeData>,
    pub snarl_zoom_pending: f32,
    pub mapping_nodes: std::collections::HashMap<usize, MappingNodeIds>,
}

pub struct MappingNodeIds {
    pub trigger: egui_snarl::NodeId,
    pub action: egui_snarl::NodeId,
    pub outputs: Vec<egui_snarl::NodeId>,
}

impl BridgeApp {
    pub fn new(
        midi: Arc<Mutex<Midi>>,
        config: BridgeConfig,
        tx_system: mpsc::Sender<SystemCommand>,
    ) -> Self {
        let mut app = Self {
            midi: midi.clone(),
            last_applied_config: config.clone(),
            eos_ip_edit: config.eos_ip.clone(),
            controller_profile_edit: config.controller_profile.clone(),
            config_edit: config,
            status_message: "Ready".to_string(),
            tx_system,
            file_dialog: egui_file_dialog::FileDialog::new(),
            active_tab: Tab::NodeGraph,
            snarl: egui_snarl::Snarl::new(),
            snarl_zoom_pending: 1.0,
            mapping_nodes: std::collections::HashMap::new(),
        };

        // Pre-populate the node graph with the current profile
        if let Ok(m) = midi.lock() {
            app.populate_snarl(&m.profile);
        }

        app
    }
}

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // --- Activity Log Consumption ---
        if let Ok(mut m) = self.midi.lock() {
            if !m.activity_log.is_empty() {
                let logs: Vec<_> = m.activity_log.drain(..).collect();
                log::debug!("Received {} activity events", logs.len());
                for event in logs {
                    log::debug!(
                        "Activity: mapping_idx={}, part={:?}, value={}",
                        event.mapping_idx,
                        event.part,
                        event.value
                    );

                    if let Some(node_ids) = self.mapping_nodes.get(&event.mapping_idx) {
                        let target_node_id = match event.part {
                            eos_midi_bridge::ActivityPart::Trigger => Some(node_ids.trigger),
                            eos_midi_bridge::ActivityPart::Action => Some(node_ids.action),
                            eos_midi_bridge::ActivityPart::Output(i) => {
                                node_ids.outputs.get(i).copied()
                            }
                        };

                        if let Some(node_id) = target_node_id {
                            if let Some(node) = self.snarl.get_node_mut(node_id) {
                                let live_state = match node {
                                    eos_midi_bridge::nodes::NodeData::Trigger(_, s) => s,
                                    eos_midi_bridge::nodes::NodeData::Action(_, s) => s,
                                    eos_midi_bridge::nodes::NodeData::Output(_, s) => s,
                                };
                                live_state.last_value = event.value.clone();
                                live_state.last_activity = Some(std::time::Instant::now());
                                log::debug!(
                                    "Updated node {:?} with value: {}",
                                    node_id,
                                    event.value
                                );
                            }
                        } else {
                            log::warn!(
                                "No node_id found for mapping_idx={}, part={:?}",
                                event.mapping_idx,
                                event.part
                            );
                        }
                    } else {
                        log::warn!("No mapping found for mapping_idx={}", event.mapping_idx);
                    }
                }
                ctx.request_repaint();
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let mut populate_needed = None;

            // Header with App Title and EOS Status
            ui.horizontal(|ui| {
                ui.heading("Eos Mackie Bridge");

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

            ui.add_space(5.0);
            ui.separator();

            // Tab Navigation
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, Tab::NodeGraph, "🕸 Node Graph");
                ui.selectable_value(&mut self.active_tab, Tab::Console, "🎛 Console");
                ui.selectable_value(&mut self.active_tab, Tab::Settings, "⚙ Settings");
            });

            ui.separator();
            ui.add_space(10.0);

            // Tab Content
            match self.active_tab {
                Tab::Console => {
                    // Fader levels
                    match self.midi.lock() {
                        Ok(m) => {
                            ui.horizontal(|ui| {
                                ui.heading(format!("Bank: {}", m.fader_page));
                                ui.label(format!("(Profile: {})", m.profile.name));
                            });
                            ui.add_space(10.0);

                            ui.horizontal_top(|ui| {
                                let bank_size = m.profile.eos_bank_size;
                                for i in 0..bank_size {
                                    let column_width = 75.0;
                                    ui.allocate_ui(egui::vec2(column_width, 220.0), |ui| {
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

                                            let val = m.fader_levels.get(i).copied().unwrap_or(0.0);
                                            let mut val_mut = val;
                                            let slider = egui::Slider::new(&mut val_mut, 0.0..=1.0)
                                                .vertical()
                                                .show_value(false);

                                            ui.add_enabled_ui(false, |ui| {
                                                ui.horizontal(|ui| {
                                                    let slider_width = 22.0;
                                                    let padding =
                                                        (column_width - slider_width) / 2.0;
                                                    ui.add_space(padding);
                                                    ui.add_sized([slider_width, 160.0], slider);
                                                });
                                            });

                                            ui.add_space(4.0);

                                            ui.add_sized(
                                                [column_width, 15.0],
                                                egui::Label::new(
                                                    egui::RichText::new(format!(
                                                        "{:.0}%",
                                                        val * 100.0
                                                    ))
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

                    // Active Cue Display
                    ui.add_space(20.0);
                    ui.separator();
                    ui.add_space(10.0);

                    ui.vertical_centered(|ui| {
                        match self.midi.lock() {
                            Ok(m) => {
                                let cue_text = if let Some(cue) = m.current_cue {
                                    format!("ACTIVE CUE: {:.2}", cue)
                                } else {
                                    "ACTIVE CUE: --".to_string()
                                };

                                ui.add(egui::Label::new(
                                    egui::RichText::new(cue_text).heading().strong().color(
                                        if m.current_cue.is_some() {
                                            egui::Color32::from_rgb(255, 215, 0) // Gold
                                        } else {
                                            egui::Color32::GRAY
                                        },
                                    ),
                                ));
                            }
                            Err(_) => {
                                ui.label("⚠ Unable to read cue");
                            }
                        }
                    });
                }
                Tab::Settings => {
                    ui.columns(2, |columns| {
                        columns[0].vertical(|ui| {
                            ui.strong("🌐 OSC Settings");
                            ui.add_space(10.0);
                            egui::Grid::new("osc_grid")
                                .num_columns(2)
                                .spacing([10.0, 10.0])
                                .show(ui, |ui| {
                                    ui.label("Eos IP:");
                                    if ui.text_edit_singleline(&mut self.eos_ip_edit).changed() {
                                        self.config_edit.eos_ip = self.eos_ip_edit.clone();
                                    }
                                    ui.end_row();

                                    ui.label("Eos OSC Port:");
                                    ui.add(egui::DragValue::new(
                                        &mut self.config_edit.eos_osc_port,
                                    ));
                                    ui.end_row();

                                    ui.label("Bridge Listen Port:");
                                    ui.add(egui::DragValue::new(
                                        &mut self.config_edit.bridge_listen_port,
                                    ));
                                    ui.end_row();
                                });
                        });

                        columns[1].vertical(|ui| {
                            ui.strong("🎹 MIDI & Profile Settings");
                            ui.add_space(10.0);
                            egui::Grid::new("midi_grid")
                                .num_columns(2)
                                .spacing([10.0, 10.0])
                                .show(ui, |ui| {
                                    if let Ok(m) = self.midi.lock() {
                                        ui.label("MIDI Input:");
                                        let mut selected_in = self.config_edit.midi_in_name.clone();
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
                                                    ui.selectable_value(
                                                        &mut selected_in,
                                                        Some(port.clone()),
                                                        port,
                                                    );
                                                }
                                            });
                                        self.config_edit.midi_in_name = selected_in;
                                        ui.end_row();

                                        ui.label("MIDI Output:");
                                        let mut selected_out =
                                            self.config_edit.midi_out_name.clone();
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
                                                ui.selectable_value(
                                                    &mut selected_out,
                                                    None,
                                                    "None",
                                                );
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

                                        ui.label("Controller Profile:");
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new(&m.profile.name).strong());
                                            if ui.button("📂 Change...").clicked() {
                                                self.file_dialog.pick_file();
                                            }
                                        });
                                        ui.end_row();
                                    }
                                });
                        });
                    });

                    if let Some(path) = self.file_dialog.update(ctx).picked() {
                        let path_str = path.to_string_lossy().to_string();
                        self.controller_profile_edit = path_str.clone();
                        self.config_edit.controller_profile = path_str;
                    }

                    ui.add_space(20.0);
                    ui.separator();
                    ui.add_space(10.0);

                    if ui.button("💾 Save to Disk & Apply").clicked() {
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
                }
                Tab::NodeGraph => {
                    ui.horizontal(|ui| {
                        if ui.button("➕").on_hover_text("Zoom In").clicked() {
                            self.snarl_zoom_pending *= 1.25;
                        }
                        if ui.button("➖").on_hover_text("Zoom Out").clicked() {
                            self.snarl_zoom_pending *= 0.8;
                        }
                        ui.separator();
                        if ui.button("🔄 Refresh Mapping").clicked() {
                            if let Ok(m) = self.midi.lock() {
                                populate_needed = Some(m.profile.clone());
                            }
                        }
                        ui.separator();
                        ui.label("Pinch/Scroll to zoom, Drag to pan");
                    });

                    ui.separator();

                    // If snarl is empty, populate it automatically
                    if self.snarl.nodes().next().is_none() {
                        if let Ok(m) = self.midi.lock() {
                            populate_needed = Some(m.profile.clone());
                        }
                    }

                    let snarl_rect = ui.available_rect_before_wrap();
                    let mut viewer = eos_midi_bridge::nodes::NodeGraphViewer {
                        zoom_delta: self.snarl_zoom_pending,
                        zoom_center: Some(snarl_rect.center()),
                    };

                    self.snarl.show(
                        &mut viewer,
                        &egui_snarl::ui::SnarlStyle::default(),
                        egui::Id::new("snarl_graph"),
                        ui,
                    );

                    self.snarl_zoom_pending = 1.0;
                }
            }

            if let Some(profile) = populate_needed {
                self.populate_snarl(&profile);
            }

            // Bottom Status Bar
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(5.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if let Ok(m) = self.midi.lock() {
                        ui.label(egui::RichText::new(&m.connection_status).strong().small());
                        ui.label("|");
                    }
                    ui.label(egui::RichText::new(&self.status_message).small());
                });
            });
        });

        // Request repaint at a reasonable rate (30 FPS) instead of constantly
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

impl BridgeApp {
    fn populate_snarl(&mut self, profile: &eos_midi_bridge::controller::ControllerProfile) {
        use eos_midi_bridge::nodes::NodeData;
        self.snarl = egui_snarl::Snarl::new();
        self.mapping_nodes.clear();

        let total_mappings = profile.mappings.len();
        let split_idx = (total_mappings + 1) / 2;

        for (m_idx, mapping) in profile.mappings.iter().enumerate() {
            let (col_idx, row_idx) = if m_idx < split_idx {
                (0, m_idx)
            } else {
                (1, m_idx - split_idx)
            };

            let x_base = col_idx as f32 * 800.0;
            let y_base = row_idx as f32 * 150.0;

            let trigger_id = self.snarl.insert_node(
                egui::pos2(x_base + 50.0, y_base + 50.0),
                NodeData::Trigger(mapping.trigger.clone(), Default::default()),
            );
            self.snarl.open_node(trigger_id, true);

            let action_id = self.snarl.insert_node(
                egui::pos2(x_base + 300.0, y_base + 50.0),
                NodeData::Action(mapping.action.clone(), Default::default()),
            );
            self.snarl.open_node(action_id, true);

            let mut output_node_ids = Vec::new();

            self.snarl.connect(
                egui_snarl::OutPinId {
                    node: trigger_id,
                    output: 0,
                },
                egui_snarl::InPinId {
                    node: action_id,
                    input: 0,
                },
            );

            for (i, output) in mapping.outputs.iter().enumerate() {
                let output_id = self.snarl.insert_node(
                    egui::pos2(x_base + 550.0, y_base + i as f32 * 60.0),
                    NodeData::Output(output.clone(), Default::default()),
                );
                self.snarl.open_node(output_id, true);
                output_node_ids.push(output_id);

                self.snarl.connect(
                    egui_snarl::OutPinId {
                        node: action_id,
                        output: 0,
                    },
                    egui_snarl::InPinId {
                        node: output_id,
                        input: 0,
                    },
                );
            }

            self.mapping_nodes.insert(
                m_idx,
                MappingNodeIds {
                    trigger: trigger_id,
                    action: action_id,
                    outputs: output_node_ids,
                },
            );
        }
    }
}
