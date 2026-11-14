//! blurt: a small, fast, native Mumble client.

use iced::widget::{center, column, text};
use iced::{Element, Subscription, Task, window};

fn main() -> iced::Result {
    // A daemon outlives its windows, which is how close-to-tray will work.
    // Until the tray exists (milestone 5), closing the last window quits.
    iced::daemon(Blurt::new, Blurt::update, Blurt::view)
        .title(Blurt::title)
        .subscription(Blurt::subscription)
        .run()
}

struct Blurt {
    window: Option<window::Id>,
}

#[derive(Debug, Clone)]
enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
}

impl Blurt {
    fn new() -> (Self, Task<Message>) {
        let (_id, open) = window::open(window::Settings {
            size: iced::Size::new(900.0, 600.0),
            min_size: Some(iced::Size::new(360.0, 240.0)),
            ..window::Settings::default()
        });
        (Self { window: None }, open.map(Message::WindowOpened))
    }

    fn title(&self, _window: window::Id) -> String {
        "blurt".to_owned()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowOpened(id) => {
                self.window = Some(id);
                Task::none()
            }
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                // TODO(milestone 5): stay alive in the tray instead.
                iced::exit()
            }
        }
    }

    fn view(&self, _window: window::Id) -> Element<'_, Message> {
        center(column![text("blurt").size(32), text("Not connected")].spacing(8)).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        window::close_events().map(Message::WindowClosed)
    }
}
