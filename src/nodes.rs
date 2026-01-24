use crate::controller::{LogicalAction, Output, Trigger};
use eframe::egui;
use egui_snarl::{
    Snarl,
    ui::{PinInfo, SnarlViewer},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default)]
pub struct NodeLiveState {
    pub last_value: String,
    pub last_activity: Option<std::time::Instant>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum NodeData {
    Trigger(
        Trigger,
        #[serde(skip)]
        #[serde(default)]
        NodeLiveState,
    ),
    Action(
        LogicalAction,
        #[serde(skip)]
        #[serde(default)]
        NodeLiveState,
    ),
    Output(
        Output,
        #[serde(skip)]
        #[serde(default)]
        NodeLiveState,
    ),
}

pub struct NodeGraphViewer {
    pub zoom_delta: f32,
    pub zoom_center: Option<egui::Pos2>,
}

impl SnarlViewer<NodeData> for NodeGraphViewer {
    fn title(&mut self, node: &NodeData) -> String {
        match node {
            NodeData::Trigger(t, _) => match t {
                Trigger::MidiNote { note, .. } => format!("MIDI Note {}", note),
                Trigger::MidiCc { cc, .. } => format!("MIDI CC {}", cc),
                Trigger::MidiPitchwheel { .. } => "MIDI Pitchwheel".to_string(),
                Trigger::Osc { addr } => format!("OSC In: {}", addr),
            },
            NodeData::Action(a, _) => match a {
                LogicalAction::Go => "Action: GO".to_string(),
                LogicalAction::Stop => "Action: STOP".to_string(),
                LogicalAction::Resume => "Action: RESUME".to_string(),
                LogicalAction::FaderMove { index } => format!("Action: Fader {}", index + 1),
                LogicalAction::FaderPageUp => "Action: Page UP".to_string(),
                LogicalAction::FaderPageDown => "Action: Page DOWN".to_string(),
            },
            NodeData::Output(o, _) => match o {
                Output::Osc { addr, .. } => format!("OSC Out: {}", addr),
                Output::MidiNote { note, .. } => format!("MIDI LED {}", note),
                Output::MidiPitchwheel { .. } => "Fader FB".to_string(),
                Output::LcdStrip { strip, .. } => format!("LCD Strip {}", strip),
                Output::LcdText { text, .. } => format!("LCD Text: {}", text),
            },
        }
    }

    fn show_body(
        &mut self,
        node_id: egui_snarl::NodeId,
        _inputs: &[egui_snarl::InPin],
        _outputs: &[egui_snarl::OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<NodeData>,
    ) {
        let node = &snarl[node_id];
        let live_state = match node {
            NodeData::Trigger(_, s) => s,
            NodeData::Action(_, s) => s,
            NodeData::Output(_, s) => s,
        };

        ui.vertical(|ui| {
            // Show Live Value if present with highlight if active
            if !live_state.last_value.is_empty() {
                let elapsed = live_state
                    .last_activity
                    .map(|last| last.elapsed().as_millis());
                let is_active = elapsed.is_some_and(|e| e < 300);

                if is_active {
                    // Request continuous repaint while flashing
                    ui.ctx().request_repaint();
                }

                let text_color = if is_active {
                    match node {
                        NodeData::Trigger(..) => egui::Color32::from_rgb(0, 255, 0),
                        NodeData::Action(..) => egui::Color32::from_rgb(255, 255, 0),
                        NodeData::Output(..) => egui::Color32::from_rgb(255, 100, 100),
                    }
                } else {
                    egui::Color32::WHITE
                };

                ui.label(
                    egui::RichText::new(&live_state.last_value)
                        .strong()
                        .color(text_color),
                );
            }

            match node {
                NodeData::Trigger(t, _) => match t {
                    Trigger::MidiNote { channel, mode, .. } => {
                        ui.label(format!("Ch: {}, Mode: {:?}", *channel + 1, mode));
                    }
                    Trigger::MidiCc { channel, .. } => {
                        ui.label(format!("Ch: {}", *channel + 1));
                    }
                    Trigger::MidiPitchwheel { channel } => {
                        ui.label(format!("Ch: {}", *channel + 1));
                    }
                    Trigger::Osc { addr } => {
                        ui.label(format!("Address: {}", addr));
                    }
                },
                NodeData::Action(_, _) => {}
                NodeData::Output(o, _) => match o {
                    Output::Osc { arg_type, .. } => {
                        ui.label(format!("Arg: {}", arg_type));
                    }
                    Output::MidiNote {
                        channel, velocity, ..
                    } => {
                        ui.label(format!("Ch: {}, Vel: {}", *channel + 1, velocity));
                    }
                    Output::MidiPitchwheel { channel } => {
                        ui.label(format!("Ch: {}", *channel + 1));
                    }
                    _ => {}
                },
            }
        });
    }

    fn inputs(&mut self, node: &NodeData) -> usize {
        match node {
            NodeData::Trigger(..) => 0,
            NodeData::Action(..) => 1,
            NodeData::Output(..) => 1,
        }
    }

    fn outputs(&mut self, node: &NodeData) -> usize {
        match node {
            NodeData::Trigger(..) => 1,
            NodeData::Action(..) => 1,
            NodeData::Output(..) => 0,
        }
    }

    fn has_body(&mut self, _node: &NodeData) -> bool {
        true
    }

    fn show_input(
        &mut self,
        _pin: &egui_snarl::InPin,
        _ui: &mut egui::Ui,
        _snarl: &mut Snarl<NodeData>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        PinInfo::square().with_fill(egui::Color32::from_rgb(100, 100, 200))
    }

    fn show_output(
        &mut self,
        _pin: &egui_snarl::OutPin,
        _ui: &mut egui::Ui,
        _snarl: &mut Snarl<NodeData>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        PinInfo::triangle().with_fill(egui::Color32::from_rgb(200, 100, 100))
    }

    fn current_transform(
        &mut self,
        to_global: &mut egui::emath::TSTransform,
        _snarl: &mut Snarl<NodeData>,
    ) {
        if self.zoom_delta != 1.0 {
            if let Some(center) = self.zoom_center {
                // Zoom around specific point (e.g. viewport center)
                let old_scaling = to_global.scaling;
                let new_scaling = (old_scaling * self.zoom_delta).clamp(0.1, 10.0);

                // Formula: translation = center - (center - translation) / old_scaling * new_scaling
                // Which is equivalent to: to_global = TSTransform::from_parent_pos(center).scaling(new_scaling).translation(center.to_vec2()) ...
                // Simplified:
                let pivot_in_graph = (center - to_global.translation) / old_scaling;
                to_global.translation = center.to_vec2() - pivot_in_graph.to_vec2() * new_scaling;
                to_global.scaling = new_scaling;
            } else {
                to_global.scaling = (to_global.scaling * self.zoom_delta).clamp(0.1, 10.0);
            }
            self.zoom_delta = 1.0;
        }
    }
}
