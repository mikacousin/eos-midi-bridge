#![windows_subsystem = "windows"]
use iced::widget::{button, column, container, pick_list, progress_bar, row, text, text_input};
use iced::{
    window, Alignment, Application, Color, Command, Element, Event, Length, Settings, Theme,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
            size: iced::Size::new(900.0, 900.0),
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
    fader_levels: [f32; 9],
    fader_labels: [String; 9],
}

#[derive(Debug, Clone)]
enum Message {
    InPortSelected(String),
    OutPortSelected(String),
    ToggleBridge,
    EventOccurred(BridgeEvent),

    EosIpChanged(String),
    EosPortChanged(String),
    ListenPortChanged(String),
    SaveConfig,
    SaveResult(Result<(), String>),
    WindowClosed,
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
                fader_levels: [0.0; 9],
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
            Message::InPortSelected(port) => self.selected_in = Some(port),
            Message::OutPortSelected(port) => self.selected_out = Some(port),
            Message::ToggleBridge => {
                if self.selected_in.is_some() && self.selected_out.is_some() {
                    self.is_running = !self.is_running;
                }
            }
            Message::WindowClosed => {
                // If we are currently connected, clear the hardware display
                if let (Some(out_name), true) = (&self.selected_out, self.is_running) {
                    let midi_out = midir::MidiOutput::new("Eos-Cleanup").unwrap();
                    if let Some(port) = midi_out
                        .ports()
                        .iter()
                        .find(|p| midi_out.port_name(p).unwrap_or_default() == *out_name)
                    {
                        if let Ok(mut conn) = midi_out.connect(port, "cleanup") {
                            midi_osc_logic::clear_mcu_display(&mut conn);
                            // Brief sleep to ensure the MIDI message is sent before the process dies
                            std::thread::sleep(std::time::Duration::from_millis(1000));
                        }
                    }
                }
                // Explicity exit the process
                return iced::window::close(iced::window::Id::MAIN);
            }
            Message::EventOccurred(BridgeEvent::None) => {}
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
                // Clone the existing config and overwrite fields from UI values
                let mut new_cfg = (*self.config).clone();
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

    fn subscription(&self) -> iced::Subscription<Message> {
        let mut subs = vec![];

        // Add the window close listener
        subs.push(iced::event::listen().map(|event| match event {
            Event::Window(_, window::Event::CloseRequested) => Message::WindowClosed,
            _ => Message::EventOccurred(BridgeEvent::None),
        }));

        if self.is_running {
            if let (Some(in_p), Some(out_p)) = (&self.selected_in, &self.selected_out) {
                subs.push(
                    bridge_subscription(in_p.clone(), out_p.clone(), self.config.clone())
                        .map(Message::EventOccurred),
                );
            }
        }
        iced::Subscription::batch(subs)
    }

    fn view(&self) -> Element<'_, Message> {
        let is_connected = self
            .last_heartbeat
            .map_or(false, |t| t.elapsed() < Duration::from_secs(7));

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

        let cfg_column = container(
            column![
                text("EOS Configuration").style(EOS_GOLD),
                row![
                    text("EOS IP:").width(Length::FillPortion(1)),
                    text_input("127.0.0.1", &self.eos_ip_value)
                        .width(Length::FillPortion(2))
                        .on_input(Message::EosIpChanged)
                ]
                .align_items(Alignment::Center)
                .spacing(8),
                row![
                    text("EOS Port:").width(Length::FillPortion(1)),
                    text_input("8000", &self.eos_port_value)
                        .width(Length::FillPortion(1))
                        .on_input(Message::EosPortChanged)
                ]
                .align_items(Alignment::Center)
                .spacing(8),
                row![
                    text("Listen Port:").width(Length::FillPortion(1)),
                    text_input("8001", &self.listen_port_value)
                        .width(Length::FillPortion(1))
                        .on_input(Message::ListenPortChanged)
                ]
                .align_items(Alignment::Center)
                .spacing(8),
                button("Save Configuration").on_press(Message::SaveConfig)
            ]
            .spacing(10),
        )
        .padding(10)
        .style(move |_: &Theme| container::Appearance {
            background: Some(EOS_SURFACE.into()),
            border: iced::Border {
                width: 1.0,
                color: Color::BLACK,
                radius: 4.0.into(),
            },
            ..Default::default()
        });

        let fader_bank =
            row(self
                .fader_levels
                .iter()
                .enumerate()
                .skip(1)
                .take(8)
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
                        container(
                            progress_bar(0.0..=1.0, lvl)
                                .width(Length::Fixed(45.0))
                                .height(Length::Fixed(180.0))
                        )
                        .width(70)
                        .height(200)
                        .center_x()
                        .center_y(),
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
            column![header, setup_box, cfg_column, fader_bank]
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
