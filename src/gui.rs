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
    pub editing_device: Option<String>,
    pub tx_system: mpsc::Sender<SystemCommand>,
    pub file_dialog: egui_file_dialog::FileDialog,
    active_tab: Tab,
    pub snarl: egui_snarl::Snarl<eos_midi_bridge::nodes::NodeData>,
    pub snarl_zoom_pending: f32,
    pub mapping_nodes: std::collections::HashMap<usize, MappingNodeIds>,
    pub show_quit_confirmation: bool,
    pub allowed_to_close: bool,
    pub selected_controller_for_graph: Option<String>,
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
            editing_device: None,
            config_edit: config,
            status_message: "Ready".to_string(),
            tx_system,
            file_dialog: egui_file_dialog::FileDialog::new(),
            active_tab: Tab::Console,
            snarl: egui_snarl::Snarl::new(),
            snarl_zoom_pending: 1.0,
            mapping_nodes: std::collections::HashMap::new(),
            show_quit_confirmation: false,
            allowed_to_close: false,
            selected_controller_for_graph: None,
        };

        // Pre-populate the node graph with the current profile
        if let Ok(m) = midi.lock() {
            if let Some((name, p)) = m.device_profiles.iter().next() {
                app.selected_controller_for_graph = Some(name.clone());
                app.populate_snarl(p);
            }
        }

        app
    }
}

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Intercept Close Request (System X button)
        if ctx.input(|i| i.viewport().close_requested()) && !self.allowed_to_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_quit_confirmation = true;
        }

        // Handle Quit Shortcut (Ctrl+Q / Cmd+Q)
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Q)) {
            self.show_quit_confirmation = true;
        }

        // Render Quit Confirmation Modal
        if self.show_quit_confirmation {
            egui::Window::new("Quit Confirmation")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Are you sure you want to quit?").strong());
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            if ui.button("Cancel").clicked() {
                                self.show_quit_confirmation = false;
                            }
                            // Default action on Enter (Visualized as Primary Button)
                            let quit_btn = egui::Button::new(
                                egui::RichText::new("Quit")
                                    .color(ctx.style().visuals.strong_text_color()),
                            )
                            .fill(ctx.style().visuals.selection.bg_fill);

                            if ui.add(quit_btn).clicked()
                                || ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                self.allowed_to_close = true;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                        ui.add_space(10.0);
                    });
                });
        }

        // Disable interaction with the main app if modal is open
        let main_enabled = !self.show_quit_confirmation;

        // --- Activity Log Consumption ---
        if let Ok(mut m) = self.midi.lock()
            && !m.activity_log.is_empty()
        {
            let logs: Vec<_> = m.activity_log.drain(..).collect();
            log::debug!("Received {} activity events", logs.len());
            for event in logs {
                // Filter by selected controller to avoid cross-talk
                if let Some(selected) = &self.selected_controller_for_graph {
                    if event.device_name != *selected {
                        continue;
                    }
                } else {
                    continue;
                }

                log::debug!(
                    "Activity: device={}, mapping_idx={}, part={:?}, value={}",
                    event.device_name,
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
                            log::debug!("Updated node {:?} with value: {}", node_id, event.value);
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

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(main_enabled, |ui| {
                let mut populate_needed = None;

                // Header with App Title and EOS Status
                ui.horizontal(|ui| {
                    ui.heading("Eos Mackie Bridge");

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (status_text, color) = if let Ok(m) = self.midi.lock() {
                            match m.last_osc_heartbeat {
                                None => ("WAITING EOS", egui::Color32::GRAY),
                                Some(last)
                                    if last.elapsed() < std::time::Duration::from_secs(4) =>
                                {
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
                    ui.selectable_value(&mut self.active_tab, Tab::Console, "🎛 Simple View");
                    ui.selectable_value(&mut self.active_tab, Tab::NodeGraph, "🕸 Advanced View");
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
                                    let profile_names: Vec<String> = m.device_profiles.values().map(|p| p.name.clone()).collect();
                                    ui.label(format!("(Profiles: {})", profile_names.join(", ")));
                                });
                                ui.add_space(10.0);

                                ui.horizontal_top(|ui| {
                                    // Use max bank size for display
                                    let mut bank_size = 0;
                                    for p in m.device_profiles.values() {
                                        if p.eos_bank_size > bank_size { bank_size = p.eos_bank_size; }
                                    }
                                    if bank_size == 0 { bank_size = 10; }

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

                                                let val =
                                                    m.fader_levels.get(i).copied().unwrap_or(0.0);
                                                let mut val_mut = val;
                                                let slider =
                                                    egui::Slider::new(&mut val_mut, 0.0..=1.0)
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
                                        if ui.text_edit_singleline(&mut self.eos_ip_edit).changed()
                                        {
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
                                            ui.label("Detected Controllers:");
                                            ui.vertical(|ui| {
                                                let mut unique_devices: std::collections::HashSet<
                                                    String,
                                                > = std::collections::HashSet::new();
                                                for p in &m.available_in_ports {
                                                    unique_devices.insert(p.clone());
                                                }
                                                for p in &m.available_out_ports {
                                                    unique_devices.insert(p.clone());
                                                }
                                                let mut sorted_devices: Vec<_> =
                                                    unique_devices.into_iter().collect();
                                                sorted_devices.sort();

                                                if sorted_devices.is_empty() {
                                                    ui.label(
                                                        egui::RichText::new("No devices found")
                                                            .italics(),
                                                    );
                                                }

                                                for device in sorted_devices {
                                                    let mut enabled = self
                                                        .config_edit
                                                        .enabled_devices
                                                        .contains_key(&device);

                                                    ui.horizontal(|ui| {
                                                        if ui.checkbox(&mut enabled, &device).changed()
                                                        {
                                                            if enabled {
                                                                if !self
                                                                    .config_edit
                                                                    .enabled_devices
                                                                    .contains_key(&device)
                                                                {
                                                                    // Default profile path
                                                                    self.config_edit
                                                                        .enabled_devices
                                                                        .insert(device.clone(), "profiles/icon_platform_m_plus.json".to_string());
                                                                }
                                                            } else {
                                                                self.config_edit
                                                                    .enabled_devices
                                                                    .remove(&device);
                                                            }
                                                        }

                                                        if enabled {
                                                            let current_profile = self.config_edit.enabled_devices.get(&device).cloned().unwrap_or_default();
                                                            ui.label(egui::RichText::new(&current_profile).italics().small());
                                                            if ui.button("📂").on_hover_text("Change Profile").clicked() {
                                                                self.editing_device = Some(device.clone());
                                                                self.file_dialog.pick_file();
                                                            }
                                                        }
                                                    });
                                                }
                                            });
                                            ui.end_row();
                                        }
                                    });
                            });
                        });

                        if let Some(path) = self.file_dialog.update(ctx).picked() {
                             if let Some(device) = &self.editing_device {
                                let path_str = path.to_string_lossy().to_string();
                                self.config_edit.enabled_devices.insert(device.clone(), path_str);
                             }
                             // Reset editing device? Maybe not, keep it in case they pick again? No, usually reset.
                             self.editing_device = None;
                        }

                        ui.add_space(20.0);
                        ui.separator();
                        ui.add_space(10.0);

                        if ui.button("💾 Save to Disk & Apply").clicked() {
                            match store_config(&self.config_edit) {
                                Ok(_) => {
                                    self.status_message =
                                        "✓ Configuration saved to disk".to_string();
                                }
                                Err(e) => {
                                    error!("Failed to save configuration: {}", e);
                                    self.status_message = format!("❌ Error: {}", e);
                                }
                            }
                        }

                        // Auto-apply changes if config has changed and is valid
                        if self.config_edit != self.last_applied_config
                            && self.config_edit.validate().is_ok()
                        {
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
                    Tab::NodeGraph => {
                        // Capture input zoom (scroll/pinch) from egui's standard helper
                        // This handles Ctrl + Scroll and Pinch gestures automatically
                        let input_zoom = ui.input(|i| i.zoom_delta());
                        self.snarl_zoom_pending *= input_zoom;

                        ui.horizontal(|ui| {
                             // Controller Selector
                             if let Ok(m) = self.midi.lock() {
                                 let mut profiles: Vec<_> = m.device_profiles.keys().cloned().collect();
                                 profiles.sort();

                                 let current_sel = self.selected_controller_for_graph.clone().unwrap_or_default();
                                 let mut selected = current_sel.clone();

                                 ui.label("Controller:");
                                 egui::ComboBox::from_id_salt("graph_controller_selector")
                                     .selected_text(&selected)
                                     .show_ui(ui, |ui| {
                                         for p in profiles {
                                             ui.selectable_value(&mut selected, p.clone(), p);
                                         }
                                     });

                                 if selected != current_sel {
                                     self.selected_controller_for_graph = Some(selected.clone());
                                     if let Some(p) = m.device_profiles.get(&selected) {
                                         populate_needed = Some(p.clone());
                                     }
                                 }
                             }

                            ui.separator();

                            if ui.button("➕").on_hover_text("Zoom In").clicked() {
                                self.snarl_zoom_pending *= 1.25;
                            }
                            if ui.button("➖").on_hover_text("Zoom Out").clicked() {
                                self.snarl_zoom_pending *= 0.8;
                            }
                            ui.separator();
                            if ui.button("🔄 Refresh").clicked()
                                && let Ok(m) = self.midi.lock()
                            {
                                if let Some(name) = &self.selected_controller_for_graph
                                   && let Some(p) = m.device_profiles.get(name) {
                                     populate_needed = Some(p.clone());
                                }
                            }
                            ui.separator();
                            ui.label("Ctrl + Scroll to zoom, Drag to pan");
                        });

                        ui.separator();

                        // If snarl is empty, populate it automatically
                        if self.snarl.nodes().next().is_none()
                            && let Ok(m) = self.midi.lock()
                        {
                             // Try selected first
                             if let Some(name) = &self.selected_controller_for_graph
                                && let Some(p) = m.device_profiles.get(name)
                             {
                                 populate_needed = Some(p.clone());
                             } else if let Some((name, p)) = m.device_profiles.iter().next() {
                                 // Fallback to first
                                 self.selected_controller_for_graph = Some(name.clone());
                                 populate_needed = Some(p.clone());
                             }
                        }

                        let snarl_rect = ui.available_rect_before_wrap();

                        // Determine zoom center: prefer mouse pointer if hovering the graph, otherwise center of view
                        let zoom_center = ui
                            .input(|i| i.pointer.hover_pos())
                            .filter(|pos| snarl_rect.contains(*pos))
                            .unwrap_or(snarl_rect.center());

                        let mut viewer = eos_midi_bridge::nodes::NodeGraphViewer {
                            zoom_delta: self.snarl_zoom_pending,
                            zoom_center: Some(zoom_center),
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
        });

        // Request repaint at a reasonable rate (30 FPS) instead of constantly
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

impl BridgeApp {
    fn populate_snarl(&mut self, profile: &eos_midi_bridge::controller::ControllerProfile) {
        use eos_midi_bridge::controller::LogicalAction;
        use eos_midi_bridge::nodes::NodeData;

        self.snarl = egui_snarl::Snarl::new();
        self.mapping_nodes.clear();

        // deduplicate actions: Action -> (NodeId, Position)
        let mut action_node_map: std::collections::HashMap<
            LogicalAction,
            (egui_snarl::NodeId, egui::Pos2),
        > = std::collections::HashMap::new();

        // deduplicate outputs: (Action, Output) -> NodeId
        let mut action_output_map: std::collections::HashMap<
            (LogicalAction, eos_midi_bridge::controller::Output),
            egui_snarl::NodeId,
        > = std::collections::HashMap::new();

        // Track visual slots used per action to stack multiple triggers/outputs
        let mut action_trigger_slots: std::collections::HashMap<LogicalAction, usize> =
            std::collections::HashMap::new();
        let mut action_output_slots: std::collections::HashMap<LogicalAction, usize> =
            std::collections::HashMap::new();

        // Column trackers
        let mut col_rows = [0, 0]; // [Left Col (Buttons), Right Col (Faders)]

        for (m_idx, mapping) in profile.mappings.iter().enumerate() {
            // 1. Resolve Action Node
            let (action_id, action_pos) = *action_node_map
                .entry(mapping.action.clone())
                .or_insert_with(|| {
                    // Determine column based on action type
                    let col_idx = if let LogicalAction::FaderMove { .. } = mapping.action {
                        1 // Right Column
                    } else {
                        0 // Left Column
                    };

                    let row_idx = col_rows[col_idx];
                    col_rows[col_idx] += 1;

                    let default_x = col_idx as f32 * 1200.0;
                    let default_y = row_idx as f32 * 200.0;

                    let pos = egui::pos2(default_x + 350.0, default_y + 50.0);
                    let id = self.snarl.insert_node(
                        pos,
                        NodeData::Action(mapping.action.clone(), Default::default()),
                    );
                    self.snarl.open_node(id, true);
                    (id, pos)
                });

            // 2. Place Trigger (Relative to Action)
            let trigger_slot = *action_trigger_slots
                .entry(mapping.action.clone())
                .or_insert(0);
            let trigger_y_offset = trigger_slot as f32 * 100.0;
            *action_trigger_slots.get_mut(&mapping.action).unwrap() += 1;

            let trigger_pos = egui::pos2(action_pos.x - 300.0, action_pos.y + trigger_y_offset);

            let trigger_id = self.snarl.insert_node(
                trigger_pos,
                NodeData::Trigger(mapping.trigger.clone(), Default::default()),
            );
            self.snarl.open_node(trigger_id, true);

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

            // 3. Place Outputs (Relative to Action)
            let mut output_node_ids = Vec::new();
            for output in &mapping.outputs {
                let output_key = (mapping.action.clone(), output.clone());

                let output_id = if let Some(&id) = action_output_map.get(&output_key) {
                    id
                } else {
                    let output_slot = *action_output_slots
                        .entry(mapping.action.clone())
                        .or_insert(0);
                    let output_y_offset = output_slot as f32 * 80.0;
                    *action_output_slots.get_mut(&mapping.action).unwrap() += 1;

                    let output_pos =
                        egui::pos2(action_pos.x + 300.0, action_pos.y + output_y_offset);

                    let id = self.snarl.insert_node(
                        output_pos,
                        NodeData::Output(output.clone(), Default::default()),
                    );
                    self.snarl.open_node(id, true);
                    action_output_map.insert(output_key, id);

                    self.snarl.connect(
                        egui_snarl::OutPinId {
                            node: action_id,
                            output: 0,
                        },
                        egui_snarl::InPinId { node: id, input: 0 },
                    );
                    id
                };
                output_node_ids.push(output_id);
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
