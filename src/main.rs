#![windows_subsystem = "windows"]
use iced::widget::{button, column, container, pick_list, progress_bar, row, text};
use iced::{Alignment, Application, Color, Command, Element, Length, Settings, Theme};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod config;
mod midi_osc_logic;

use config::Config;
use midi_osc_logic::{bridge_subscription, BridgeEvent};

// --- Eos Color Palette ---
const EOS_BG: Color = Color::from_rgb(0.05, 0.05, 0.05); // Deep Charcoal
const EOS_SURFACE: Color = Color::from_rgb(0.15, 0.15, 0.15); // Lighter Surface
const EOS_GOLD: Color = Color::from_rgb(0.85, 0.65, 0.15); // Eos Selection Gold
const EOS_AMBER: Color = Color::from_rgb(0.9, 0.4, 0.0); // Warning/Indicator
const EOS_TEXT: Color = Color::from_rgb(0.9, 0.9, 0.9); // Off-White text

pub fn main() -> iced::Result {
    EosBridge::run(Settings {
        window: iced::window::Settings {
            size: iced::Size::new(900.0, 500.0),
            ..Default::default()
        },
        ..Default::default()
    })
}

struct EosBridge {
    config: Arc<Config>,
    in_ports: Vec<String>,
    out_ports: Vec<String>,
    selected_in: Option<String>,
    selected_out: Option<String>,
    is_running: bool,
    last_heartbeat: Option<Instant>,
    fader_levels: [f32; 11],
    fader_labels: [String; 11],
}

#[derive(Debug, Clone)]
enum Message {
    InPortSelected(String),
    OutPortSelected(String),
    ToggleBridge,
    EventOccurred(BridgeEvent),
}

impl Application for EosBridge {
    type Executor = iced::executor::Default;
    type Message = Message;
    type Theme = Theme;
    type Flags = ();

    fn new(_flags: ()) -> (Self, Command<Message>) {
        let cfg: Config = confy::load("eos-midi-bridge", None).unwrap_or_default();
        let midi_in = midir::MidiInput::new("Eos-In-Probe").unwrap();
        let midi_out = midir::MidiOutput::new("Eos-Out-Probe").unwrap();

        let in_ports = midi_in
            .ports()
            .iter()
            .map(|p| midi_in.port_name(p).unwrap())
            .collect();
        let out_ports = midi_out
            .ports()
            .iter()
            .map(|p| midi_out.port_name(p).unwrap())
            .collect();

        (
            Self {
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
        String::from("Eos MIDI-OSC Bridge")
    }

    fn update(&mut self, message: Message) -> Command<Message> {
        match message {
            Message::InPortSelected(port) => self.selected_in = Some(port),
            Message::OutPortSelected(port) => self.selected_out = Some(port),
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
        }
        Command::none()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        if self.is_running {
            bridge_subscription(
                self.selected_in.clone().unwrap(),
                self.selected_out.clone().unwrap(),
                self.config.clone(),
            )
            .map(Message::EventOccurred)
        } else {
            iced::Subscription::none()
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let is_connected = self
            .last_heartbeat
            .map_or(false, |t| t.elapsed() < Duration::from_secs(5));

        let status_color = if self.is_running {
            if is_connected {
                EOS_GOLD
            } else {
                EOS_AMBER
            }
        } else {
            Color::from_rgb(0.3, 0.3, 0.3)
        };

        let header = container(
            row![
                text("EOS MIDI BRIDGE").size(24).style(EOS_GOLD),
                row![
                    container(column![])
                        .width(12)
                        .height(12)
                        .style(move |_: &Theme| {
                            container::Appearance {
                                background: Some(status_color.into()),
                                border: iced::Border {
                                    radius: 6.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            }
                        }),
                    text(if !self.is_running {
                        "OFFLINE"
                    } else if is_connected {
                        "CONNECTED"
                    } else {
                        "WAITING FOR EOS..."
                    })
                    .size(14)
                    .style(status_color)
                ]
                .spacing(8)
                .align_items(Alignment::Center)
            ]
            .spacing(20)
            .align_items(Alignment::Center),
        )
        .padding(20);

        let setup_box = container(
            column![
                text("Hardware Configuration").style(EOS_GOLD),
                row![
                    column![
                        text("MIDI IN (iCon)").size(12),
                        pick_list(
                            self.in_ports.as_slice(),
                            self.selected_in.as_ref(),
                            Message::InPortSelected
                        )
                        .width(300)
                    ]
                    .spacing(5),
                    column![
                        text("MIDI OUT (iCon)").size(12),
                        pick_list(
                            self.out_ports.as_slice(),
                            self.selected_out.as_ref(),
                            Message::OutPortSelected
                        )
                        .width(300)
                    ]
                    .spacing(5),
                ]
                .spacing(20),
                button(
                    text(if self.is_running {
                        "DISCONNECT"
                    } else {
                        "CONNECT BRIDGE"
                    })
                    .width(Length::Fill)
                    .horizontal_alignment(iced::alignment::Horizontal::Center)
                )
                .on_press(Message::ToggleBridge)
                .padding(10)
            ]
            .spacing(15),
        )
        .padding(20)
        .style(move |_: &Theme| container::Appearance {
            background: Some(EOS_SURFACE.into()),
            border: iced::Border {
                width: 1.0,
                color: Color::BLACK,
                radius: 4.0.into(),
            },
            ..Default::default()
        });

        let fader_bank = row(self
            .fader_levels
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, &lvl)| {
                column![
                    container(
                        text(&self.fader_labels[i])
                            .size(11)
                            .horizontal_alignment(iced::alignment::Horizontal::Center)
                    )
                    .width(70)
                    .padding(5)
                    .style(|_: &Theme| container::Appearance {
                        background: Some(Color::BLACK.into()),
                        ..Default::default()
                    }),
                    container(progress_bar(0.0..=1.0, lvl))
                        .height(180)
                        .width(45),
                    text(format!("{:.0}%", lvl * 100.0))
                        .size(12)
                        .style(EOS_GOLD),
                ]
                .align_items(Alignment::Center)
                .spacing(8)
                .into()
            }))
        .spacing(10);

        container(
            column![header, setup_box, fader_bank]
                .spacing(30)
                .align_items(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_: &Theme| container::Appearance {
            background: Some(EOS_BG.into()),
            text_color: Some(EOS_TEXT),
            ..Default::default()
        })
        .into()
    }
}
