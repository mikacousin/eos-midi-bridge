use iced::{
    alignment::Alignment, executor, theme, Application, Color, Command, Element, Length, Subscription,
    Settings,
};
use iced::widget::{button, column, container, pick_list, row, text, text_input, Column, Row};
use std::sync::Arc;
use std::time::Instant;

mod config;
mod midi_osc_logic;

use config::Config;
use midi_osc_logic::{bridge_subscription, BridgeEvent};

const EOS_BG: Color = Color::from_rgb(0.05, 0.05, 0.05);
const EOS_SURFACE: Color = Color::from_rgb(0.15, 0.15, 0.15);
const EOS_GOLD: Color = Color::from_rgb(0.85, 0.65, 0.15);
const EOS_AMBER: Color = Color::from_rgb(0.9, 0.4, 0.0);
const EOS_TEXT: Color = Color::from_rgb(0.9, 0.9, 0.9);

pub fn main() -> iced::Result {
    EosBridge::run(Settings {
        window: iced::window::Settings {
            size: iced::Size::new(900.0, 600.0),
            ..Default::default()
        },
        ..Settings::default()
    })
}

struct EosBridge {
    config: Arc<Config>,

    // UI state for editable config
    eos_ip_value: String,
    eos_port_value: String,
    listen_port_value: String,

    // MIDI ports
    in_ports: Vec<String>,
    out_ports: Vec<String>,
    selected_in: Option<String>,
    selected_out: Option<String>,

    // bridge state
    is_running: bool,
    last_heartbeat: Option<Instant>,
    fader_levels: [f32; 11],
    fader_labels: [String; 11],
}

#[derive(Debug, Clone)]
enum Message {
    InPortSelected(Option<String>),
    OutPortSelected(Option<String>),
    ToggleBridge,
    EventOccurred(BridgeEvent),

    EosIpChanged(String),
    EosPortChanged(String),
    ListenPortChanged(String),
    SaveConfig,
    SaveResult(Result<(), String>),
}

impl Application for EosBridge {
    type Executor = executor::Default;
    type Message = Message;
    type Theme = theme::Theme;
    type Flags = ();

    fn new(_flags: ()) -> (Self, Command<Message>) {
        let cfg: Config = confy::load("eos-midi-bridge", None).unwrap_or_default();

        let midi_in = midir::MidiInput::new("Eos-In-Probe").unwrap();
        let midi_out = midir::MidiOutput::new("Eos-Out-Probe").unwrap();

        let in_ports = midi_in
            .ports()
            .iter()
            .map(|p| midi_in.port_name(p).unwrap_or_default())
            .collect();
        let out_ports = midi_out
            .ports()
            .iter()
            .map(|p| midi_out.port_name(p).unwrap_or_default())
            .collect();

        (
            Self {
                eos_ip_value: cfg.eos_ip.clone(),
                eos_port_value: cfg.eos_port.to_string(),
                listen_port_value: cfg.listen_port.to_string(),
                config: Arc::new(cfg),
                in_ports,
                out_ports,
                selected_in: None,
                selected_out: None,
                is_running: false,
                last_heartbeat: None,
                fader_levels: [0.0; 11],
                fader_labels: std::array::from_fn(|_| String::from("...")),
            },
            Command::none(),
        )
    }

    fn title(&self) -> String {
        "Eos MIDI-OSC Bridge".into()
    }

    fn update(&mut self, message: Message) -> Command<Message> {
        match message {
            Message::InPortSelected(opt) => self.selected_in = opt,
            Message::OutPortSelected(opt) => self.selected_out = opt,
            Message::ToggleBridge => {
                if self.selected_in.is_some() && self.selected_out.is_some() {
                    self.is_running = !self.is_running;
                }
            }
            Message::EventOccurred(event) => match event {
                BridgeEvent::ConnectionHeartbeat => self.last_heartbeat = Some(Instant::now()),
                BridgeEvent::FaderUpdate(i, v) => {
                    if (i as usize) < self.fader_levels.len() {
                        self.fader_levels[i as usize] = v;
                    }
                }
                BridgeEvent::LabelUpdate(i, l) => {
                    if (i as usize) < self.fader_labels.len() {
                        self.fader_labels[i as usize] = l;
                    }
                }
                _ => {}
            },
            Message::EosIpChanged(s) => self.eos_ip_value = s,
            Message::EosPortChanged(s) => self.eos_port_value = s,
            Message::ListenPortChanged(s) => self.listen_port_value = s,
            Message::SaveConfig => {
                let mut new_cfg = (*self.config).as_ref().clone();
                new_cfg.eos_ip = self.eos_ip_value.clone();
                if let Ok(p) = self.eos_port_value.parse::<u16>() {
                    new_cfg.eos_port = p;
                }
                if let Ok(lp) = self.listen_port_value.parse::<u16>() {
                    new_cfg.listen_port = lp;
                }

                let cfg_clone = new_cfg.clone();
                return Command::perform(
                    async move {
                        confy::store("eos-midi-bridge", None, &cfg_clone)
                            .map_err(|e| format!("failed to save config: {}", e))
                    },
                    Message::SaveResult,
                );
            }
            Message::SaveResult(res) => match res {
                Ok(_) => {
                    let updated_cfg: Config =
                        confy::load("eos-midi-bridge", None).unwrap_or_default();
                    self.config = Arc::new(updated_cfg);
                }
                Err(e) => eprintln!("Config save error: {}", e),
            },
        }

        Command::none()
    }

    fn view(&self) -> Element<Message> {
        let ports_column = column![
            text("MIDI Input:").color(EOS_TEXT),
            pick_list(
                self.in_ports.clone(),
                self.selected_in.clone(),
                Message::InPortSelected
            )
            .placeholder("Select MIDI In"),
            text("MIDI Output:").color(EOS_TEXT),
            pick_list(
                self.out_ports.clone(),
                self.selected_out.clone(),
                Message::OutPortSelected
            )
            .placeholder("Select MIDI Out"),
            button(if self.is_running { "Stop Bridge" } else { "Start Bridge" })
                .on_press(Message::ToggleBridge)
        ]
        .spacing(10)
        .padding(10);

        let cfg_column = column![
            text("EOS Configuration").size(18).color(EOS_GOLD),
            row![
                text("EOS IP:").width(Length::FillPortion(1)).color(EOS_TEXT),
                text_input("127.0.0.1", &self.eos_ip_value, Message::EosIpChanged)
                    .width(Length::FillPortion(2))
            ]
            .align_items(Alignment::Center)
            .spacing(8),
            row![
                text("EOS Port:").width(Length::FillPortion(1)).color(EOS_TEXT),
                text_input("8000", &self.eos_port_value, Message::EosPortChanged)
                    .width(Length::FillPortion(1))
            ]
            .align_items(Alignment::Center)
            .spacing(8),
            row![
                text("Listen Port:").width(Length::FillPortion(1)).color(EOS_TEXT),
                text_input("8001", &self.listen_port_value, Message::ListenPortChanged)
                    .width(Length::FillPortion(1))
            ]
            .align_items(Alignment::Center)
            .spacing(8),
            button("Save Configuration").on_press(Message::SaveConfig)
        ]
        .spacing(10)
        .padding(10);

        let content = row![ports_column, cfg_column].spacing(20).padding(10);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x()
            .center_y()
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        if self.is_running {
            if let (Some(in_name), Some(out_name)) = (self.selected_in.clone(), self.selected_out.clone())
            {
                return bridge_subscription(in_name, out_name, self.config.clone()).map(Message::EventOccurred);
            }
        }
        Subscription::none()
    }
}
