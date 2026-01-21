use crate::controller::{LogicalAction, Output, Trigger};
use eframe::egui;
use egui_snarl::{
    Snarl,
    ui::{PinInfo, SnarlViewer},
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum NodeData {
    Trigger(Trigger),
    Action(LogicalAction),
    Output(Output),
}

pub struct NodeGraphViewer {
    pub zoom_delta: f32,
    pub zoom_center: Option<egui::Pos2>,
}

impl SnarlViewer<NodeData> for NodeGraphViewer {
    fn title(&mut self, node: &NodeData) -> String {
        match node {
            NodeData::Trigger(t) => match t {
                Trigger::MidiNote { note, .. } => format!("MIDI Note {}", note),
                Trigger::MidiCc { cc, .. } => format!("MIDI CC {}", cc),
                Trigger::MidiPitchwheel { .. } => "MIDI Pitchwheel".to_string(),
                Trigger::Osc { addr } => format!("OSC In: {}", addr),
            },
            NodeData::Action(a) => match a {
                LogicalAction::Go => "Action: GO".to_string(),
                LogicalAction::Stop => "Action: STOP".to_string(),
                LogicalAction::Resume => "Action: RESUME".to_string(),
                LogicalAction::FaderMove { index } => format!("Action: Fader {}", index + 1),
                LogicalAction::MasterFaderMove => "Action: Master Fader".to_string(),
                LogicalAction::FaderPageUp => "Action: Page UP".to_string(),
                LogicalAction::FaderPageDown => "Action: Page DOWN".to_string(),
            },
            NodeData::Output(o) => match o {
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
        ui.vertical(|ui| match node {
            NodeData::Trigger(t) => match t {
                Trigger::MidiNote { channel, mode, .. } => {
                    ui.label(format!("Ch: {}, Mode: {:?}", *channel + 1, mode));
                }
                Trigger::MidiCc { channel, .. } => {
                    ui.label(format!("Ch: {}", *channel + 1));
                }
                Trigger::MidiPitchwheel { channel } => {
                    ui.label(format!("Ch: {}", *channel + 1));
                }
                Trigger::Osc { .. } => {}
            },
            NodeData::Action(_) => {}
            NodeData::Output(o) => match o {
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
        });
    }

    fn inputs(&mut self, node: &NodeData) -> usize {
        match node {
            NodeData::Trigger(_) => 0,
            NodeData::Action(_) => 1,
            NodeData::Output(_) => 1,
        }
    }

    fn outputs(&mut self, node: &NodeData) -> usize {
        match node {
            NodeData::Trigger(_) => 1,
            NodeData::Action(_) => 1,
            NodeData::Output(_) => 0,
        }
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
