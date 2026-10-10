//! The parts of a Telegram update the bot looks at.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Update {
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub message: Option<Message>,
    pub data: Option<String>,
}

impl Update {
    /// The chat the update belongs to and the user who caused it.
    pub fn origin(&self) -> Option<(i64, i64)> {
        if let Some(message) = &self.message {
            return Some((message.chat.id, message.from.as_ref()?.id));
        }
        let query = self.callback_query.as_ref()?;
        Some((query.message.as_ref()?.chat.id, query.from.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_comes_from_the_message_or_the_pressed_button() {
        let message: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":5,"date":0,"chat":{"id":-42,"type":"private"},
                "from":{"id":7,"is_bot":false,"first_name":"A"},"text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(message.origin(), Some((-42, 7)));

        let callback: Update = serde_json::from_str(
            r#"{"update_id":2,"callback_query":{"id":"cb","from":{"id":8},"data":"p:x:a",
                "message":{"message_id":6,"chat":{"id":99}}}}"#,
        )
        .unwrap();
        assert_eq!(callback.origin(), Some((99, 8)));

        let other: Update = serde_json::from_str(r#"{"update_id":3,"edited_message":{}}"#).unwrap();
        assert_eq!(other.origin(), None);
    }
}
