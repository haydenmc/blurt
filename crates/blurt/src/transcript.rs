//! The chat as shown: messages, plus a line when someone joins or leaves
//! your channel. Names are taken when things happen, so a message keeps its
//! author after they leave.

use std::collections::BTreeMap;

use chrono::{DateTime, Local, Utc};
use mumble_protocol::state::{MumbleState, TextMessage};

use crate::html;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Message {
        /// `None` for the server's own messages.
        author: Option<String>,
        time: DateTime<Local>,
        body: String,
    },
    Joined {
        name: String,
        time: DateTime<Local>,
    },
    Left {
        name: String,
        time: DateTime<Local>,
    },
}

#[derive(Debug, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    /// How many of the state's chat messages are already in `entries`.
    seen: usize,
    /// Your channel, and who was in it at the last sync.
    channel: Option<u32>,
    members: BTreeMap<u32, String>,
}

/// What a [`Transcript::sync`] added, for notification sounds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct News {
    /// Someone came into your channel.
    pub joined: bool,
    /// Someone left your channel.
    pub left: bool,
    /// A chat message from someone else (or the server).
    pub message: bool,
}

impl Transcript {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Catches up with `state`: appends new messages, and joins and leaves
    /// in your channel since the last sync. Changing channel yourself
    /// doesn't list everyone already there as joining.
    pub fn sync(&mut self, state: &MumbleState, now: DateTime<Utc>) -> News {
        let mut news = News::default();
        let me = state.current_user().map(|u| u.session);
        let messages = state.chat_messages();
        if messages.len() < self.seen {
            // The state was reset (e.g. a new connection).
            self.seen = 0;
        }
        for message in &messages[self.seen..] {
            news.message |= match message {
                TextMessage::ChatTextMessage(m) => m.actor.is_none() || m.actor != me,
                TextMessage::ServerTextMessage(_) => true,
            };
            self.entries.push(match message {
                TextMessage::ChatTextMessage(m) => Entry::Message {
                    author: Some(
                        m.actor
                            .and_then(|s| state.users.get(&s))
                            .map_or_else(|| "someone".to_owned(), |u| u.name.clone()),
                    ),
                    time: m.timestamp.with_timezone(&Local),
                    body: html::to_text(&m.content),
                },
                TextMessage::ServerTextMessage(m) => Entry::Message {
                    author: None,
                    time: m.timestamp.with_timezone(&Local),
                    body: html::to_text(&m.content),
                },
            });
        }
        self.seen = messages.len();

        let Some(me) = state.current_user() else {
            return news;
        };
        let channel = me.channel_id;
        let members: BTreeMap<u32, String> = state
            .users
            .values()
            .filter(|u| u.channel_id == channel && !u.is_me)
            .map(|u| (u.session, u.name.clone()))
            .collect();
        if self.channel == Some(channel) {
            let time = now.with_timezone(&Local);
            for (session, name) in &self.members {
                if !members.contains_key(session) {
                    news.left = true;
                    self.entries.push(Entry::Left {
                        name: name.clone(),
                        time,
                    });
                }
            }
            for (session, name) in &members {
                if !self.members.contains_key(session) {
                    news.joined = true;
                    self.entries.push(Entry::Joined {
                        name: name.clone(),
                        time,
                    });
                }
            }
        }
        self.channel = Some(channel);
        self.members = members;
        news
    }
}

/// `9:40`, in local time.
pub fn clock(time: &DateTime<Local>) -> String {
    time.format("%-H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mumble_protocol::state::{ChatTextMessage, User};

    fn user(session: u32, name: &str, channel_id: u32, is_me: bool) -> User {
        User {
            session,
            name: name.into(),
            channel_id,
            is_me,
            ..Default::default()
        }
    }

    fn state(users: &[User]) -> MumbleState {
        let mut state = MumbleState::default();
        for u in users {
            state.users.insert(u.session, u.clone());
            if u.is_me {
                state.current_user_session = Some(u.session);
            }
        }
        state
    }

    fn names(t: &Transcript) -> Vec<String> {
        t.entries()
            .iter()
            .map(|e| match e {
                Entry::Joined { name, .. } => format!("+{name}"),
                Entry::Left { name, .. } => format!("-{name}"),
                Entry::Message { author, body, .. } => {
                    format!("{}: {body}", author.as_deref().unwrap_or("server"))
                }
            })
            .collect()
    }

    #[test]
    fn joins_and_leaves_in_your_channel() {
        let now = Utc::now();
        let me = user(1, "me", 0, true);
        let mut t = Transcript::default();
        // Arriving doesn't list everyone already there.
        t.sync(&state(&[me.clone(), user(2, "kestrel", 0, false)]), now);
        assert!(t.entries().is_empty());
        t.sync(
            &state(&[
                me.clone(),
                user(2, "kestrel", 0, false),
                user(3, "wren", 0, false),
                user(4, "elsewhere", 5, false),
            ]),
            now,
        );
        t.sync(&state(&[me.clone(), user(3, "wren", 0, false)]), now);
        assert_eq!(names(&t), ["+wren", "-kestrel"]);
    }

    #[test]
    fn changing_channel_is_not_everyone_joining() {
        let now = Utc::now();
        let mut t = Transcript::default();
        t.sync(&state(&[user(1, "me", 0, true)]), now);
        t.sync(
            &state(&[user(1, "me", 7, true), user(2, "kestrel", 7, false)]),
            now,
        );
        assert!(t.entries().is_empty());
    }

    #[test]
    fn messages_are_appended_once_with_their_author() {
        let now = Utc::now();
        let mut s = state(&[user(1, "me", 0, true), user(2, "kestrel", 0, false)]);
        s.chat_messages
            .push(TextMessage::ChatTextMessage(ChatTextMessage {
                timestamp: now,
                actor: Some(2),
                session: vec![],
                content: "hi &amp; hello".into(),
            }));
        let mut t = Transcript::default();
        t.sync(&s, now);
        t.sync(&s, now);
        assert_eq!(names(&t), ["kestrel: hi & hello"]);
        // The author leaving doesn't change who said it.
        s.users.remove(&2);
        t.sync(&s, now);
        assert_eq!(names(&t), ["kestrel: hi & hello", "-kestrel"]);
    }

    #[test]
    fn news_is_what_others_did() {
        let now = Utc::now();
        let me = user(1, "me", 0, true);
        let mut t = Transcript::default();
        // Arriving is no news.
        let news = t.sync(&state(&[me.clone(), user(2, "kestrel", 0, false)]), now);
        assert_eq!(news, News::default());
        let news = t.sync(&state(&[me.clone(), user(3, "wren", 0, false)]), now);
        assert!(news.joined && news.left && !news.message);
        // Your own message isn't news; someone else's is.
        let mut s = state(&[me.clone(), user(3, "wren", 0, false)]);
        let say = |actor| {
            TextMessage::ChatTextMessage(ChatTextMessage {
                timestamp: now,
                actor: Some(actor),
                session: vec![],
                content: "hi".into(),
            })
        };
        s.chat_messages.push(say(1));
        assert_eq!(t.sync(&s, now), News::default());
        s.chat_messages.push(say(3));
        assert!(t.sync(&s, now).message);
        assert_eq!(t.sync(&s, now), News::default());
    }
}
