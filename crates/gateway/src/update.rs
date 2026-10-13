//! The parts of a Telegram update the bot looks at, and the session each belongs to.
//! Also the one thing Telegram says to a call that the bot acts on, see [`retry_after`].

use serde::Deserialize;

use crate::render;

#[derive(Debug, Deserialize)]
pub struct Update {
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
    /// A message that summons the bot as a guest: it mentions the bot, or replies to one
    /// of its messages, in a chat the bot is not a member of.
    pub guest_message: Option<Message>,
    /// The Stop button of a draft was pressed.
    pub stopped_message_generation: Option<StoppedGeneration>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    /// What a guest message is answered through, once.
    pub guest_query_id: Option<String>,
    pub reply_to_message: Option<Replied>,
    /// The part of that message which was picked out to reply to.
    pub quote: Option<Quote>,
}

/// The message another one replies to, as far as it can be put into words.
#[derive(Debug, Deserialize)]
pub struct Replied {
    pub from: Option<User>,
    pub text: Option<String>,
    /// What is written under a picture or a file.
    pub caption: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Quote {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// `private`, `group`, `supergroup` or `channel`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

impl Chat {
    /// Whether this is the chat of one user with the bot.
    pub fn is_private(&self) -> bool {
        self.kind.as_deref() == Some("private")
    }
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
    pub first_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    /// The message the button is on, if it is one in a chat the bot is in.
    pub message: Option<Message>,
    /// What stands in for it when the button is on the reply to a guest summon.
    pub inline_message_id: Option<String>,
    pub data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StoppedGeneration {
    pub chat: Chat,
    pub draft_id: i64,
}

/// How the refusal of a call that came too soon after others begins.
pub const TOO_MANY: &str = "Too Many Requests";
/// What stands between that and the seconds to wait.
const RETRY_AFTER: &str = ": retry after ";

/// What the error of such a call says when Telegram asked to be left alone for
/// `seconds`: "Too Many Requests: retry after 35". Those are Telegram's words for it,
/// but the bot writes them itself, from the figure that comes beside them, so that
/// [`retry_after`] reads back what was written here and not a description that may be
/// worded another way one day.
pub fn too_many_requests(seconds: u64) -> String {
    format!("{TOO_MANY}{RETRY_AFTER}{seconds}")
}

/// For how many seconds Telegram asks to be left alone, if `refusal` is the error of a
/// call that came too soon after others, see [`too_many_requests`].
pub fn retry_after(refusal: &str) -> Option<u64> {
    let (_, rest) = refusal.rsplit_once(TOO_MANY)?;
    rest.strip_prefix(RETRY_AFTER)?.trim().parse().ok()
}

/// Where an update belongs and who caused it.
#[derive(Debug, PartialEq, Eq)]
pub struct Origin {
    /// The name of the session's Durable Object.
    pub session: String,
    pub user_id: i64,
}

/// The session of a chat the bot is in.
pub fn chat_session(chat_id: i64) -> String {
    format!("tg:{chat_id}")
}

/// The session of one user's summons from one chat the bot is not in. It is nobody
/// else's: other people in that chat have theirs, and so has the same user elsewhere.
///
/// A chat's id alone would not do even for a session all of a chat shared. Where the bot
/// is a guest, a chat can have the id of one the bot is in and be another: the private
/// chat of two people goes by the id of one of them, which is also the id of that
/// person's own chat with the bot.
pub fn guest_session(chat_id: i64, user_id: i64) -> String {
    format!("tg:guest:{chat_id}:{user_id}")
}

impl Message {
    /// The message this one replies to: who wrote it, and the part of it that was picked
    /// out, or all of it. `None` if it replies to nothing that has words.
    pub fn replied(&self) -> Option<(Option<&str>, &str)> {
        let replied = self.reply_to_message.as_ref()?;
        let words = match &self.quote {
            Some(quote) => quote.text.as_str(),
            None => replied.text.as_deref().or(replied.caption.as_deref())?,
        };
        let author = replied
            .from
            .as_ref()
            .and_then(|user| user.first_name.as_deref());
        (!words.trim().is_empty()).then_some((author, words))
    }
}

impl Update {
    /// The session the update is for and the user who caused it. `None` for an update
    /// the bot does nothing with.
    pub fn origin(&self) -> Option<Origin> {
        let origin = |session, user_id| Some(Origin { session, user_id });
        if let Some(message) = &self.message {
            return origin(chat_session(message.chat.id), message.from.as_ref()?.id);
        }
        if let Some(message) = &self.guest_message {
            let user_id = message.from.as_ref()?.id;
            return origin(guest_session(message.chat.id, user_id), user_id);
        }
        if let Some(stopped) = &self.stopped_message_generation {
            // A draft is shown in a private chat only, and that has its user's id.
            return origin(chat_session(stopped.chat.id), stopped.chat.id);
        }
        let query = self.callback_query.as_ref()?;
        let user_id = query.from.id;
        if let Some(message) = &query.message {
            return origin(chat_session(message.chat.id), user_id);
        }
        // A button on a guest's reply comes without the chat. Its data names it, and
        // the session is that of whoever pressed: for anyone but the user who summoned
        // the bot, that is one which knows nothing of the button.
        query.inline_message_id.as_ref()?;
        let (chat_id, _) = render::guest_chat(query.data.as_deref()?)?;
        origin(guest_session(chat_id, user_id), user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(json: &str) -> Update {
        serde_json::from_str(json).unwrap()
    }

    fn origin(session: &str, user_id: i64) -> Option<Origin> {
        Some(Origin {
            session: session.into(),
            user_id,
        })
    }

    #[test]
    fn origin_comes_from_the_message_or_the_pressed_button() {
        let message = update(
            r#"{"update_id":1,"message":{"message_id":5,"date":0,"chat":{"id":-42,"type":"group"},
                "from":{"id":7,"is_bot":false,"first_name":"A"},"text":"hi"}}"#,
        );
        assert_eq!(message.origin(), origin("tg:-42", 7));
        assert!(!message.message.unwrap().chat.is_private());

        let callback = update(
            r#"{"update_id":2,"callback_query":{"id":"cb","from":{"id":8},"data":"p:x:a",
                "message":{"message_id":6,"chat":{"id":99}}}}"#,
        );
        assert_eq!(callback.origin(), origin("tg:99", 8));

        let other = update(r#"{"update_id":3,"edited_message":{}}"#);
        assert_eq!(other.origin(), None);
    }

    #[test]
    fn a_guest_summon_belongs_to_a_session_of_that_user_in_that_chat() {
        let summon = |chat: i64, user: i64| {
            update(&format!(
                r#"{{"update_id":4,"guest_message":{{"message_id":9,"date":0,
                    "chat":{{"id":{chat},"type":"supergroup"}},"guest_query_id":"gq",
                    "from":{{"id":{user},"first_name":"A"}},"text":"@bot hi"}}}}"#
            ))
        };
        assert_eq!(summon(-1001, 7).origin(), origin("tg:guest:-1001:7", 7));
        // Someone else in the same chat, and the same user in another, have their own.
        assert_eq!(summon(-1001, 8).origin(), origin("tg:guest:-1001:8", 8));
        assert_eq!(summon(-1002, 7).origin(), origin("tg:guest:-1002:7", 7));
        // In the private chat of two people the chat can carry the id of one of them,
        // whose own chat with the bot is still another session.
        assert_eq!(summon(8, 7).origin(), origin("tg:guest:8:7", 7));
        assert_ne!(summon(8, 7).origin().unwrap().session, chat_session(8));
    }

    #[test]
    fn a_button_on_a_guests_reply_leads_to_the_session_of_whoever_pressed_it() {
        let press = |user: i64, data: &str| {
            update(&format!(
                r#"{{"update_id":5,"callback_query":{{"id":"cb","from":{{"id":{user}}},
                    "inline_message_id":"im","chat_instance":"ci","data":"{data}"}}}}"#
            ))
        };
        let data = render::for_guest(
            serde_json::json!({ "inline_keyboard": [[{ "text": "Allow", "callback_data": "p:x:a" }]] }),
            -1001,
        )["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(press(7, &data).origin(), origin("tg:guest:-1001:7", 7));
        // Not the session that asked, so there the press finds nothing to answer.
        assert_eq!(press(8, &data).origin(), origin("tg:guest:-1001:8", 8));
        // A button that names no chat cannot be on a guest's reply.
        assert_eq!(press(7, "p:x:a").origin(), None);
    }

    #[test]
    fn a_call_refused_for_coming_too_often_says_how_long_to_wait() {
        let refused = "telegram editMessageText: Too Many Requests: retry after 35";
        assert_eq!(retry_after(refused), Some(35));
        assert_eq!(retry_after("Too Many Requests: retry after 4\n"), Some(4));
        assert_eq!(
            retry_after("telegram editMessageText: Bad Request: MESSAGE_ID_INVALID"),
            None
        );
        assert_eq!(retry_after("Too Many Requests: retry after soon"), None);
        assert_eq!(retry_after(""), None);
        // What the bot writes for such a refusal is what is read back here.
        let written = format!("telegram sendMessage: {}", too_many_requests(120));
        assert_eq!(retry_after(&written), Some(120));
        assert!(written.contains(TOO_MANY));
    }

    #[test]
    fn a_stopped_draft_belongs_to_the_private_chat_it_was_shown_in() {
        let stopped = update(
            r#"{"update_id":6,"stopped_message_generation":{"chat":{"id":7,"type":"private"},
                "draft_id":31}}"#,
        );
        assert_eq!(stopped.origin(), origin("tg:7", 7));
        assert_eq!(stopped.stopped_message_generation.unwrap().draft_id, 31);
    }

    #[test]
    fn what_a_message_replies_to_is_the_quoted_part_or_all_of_it() {
        let message = |rest: &str| {
            let update = update(&format!(
                r#"{{"guest_message":{{"message_id":1,"chat":{{"id":1}},"text":"@bot why?"{rest}}}}}"#
            ));
            update.guest_message.unwrap()
        };
        assert_eq!(message("").replied(), None);

        let whole = message(
            r#","reply_to_message":{"message_id":2,"chat":{"id":1},"date":0,
                "from":{"id":3,"first_name":"Bea"},"text":"It rained all week."}"#,
        );
        assert_eq!(whole.replied(), Some((Some("Bea"), "It rained all week.")));

        let part = message(
            r#","quote":{"text":"all week","position":10},
               "reply_to_message":{"from":{"id":3},"caption":"It rained all week."}"#,
        );
        assert_eq!(part.replied(), Some((None, "all week")));

        // A picture without words is nothing that can be handed on.
        let wordless = message(r#","reply_to_message":{"from":{"id":3,"first_name":"Bea"}}"#);
        assert_eq!(wordless.replied(), None);
    }
}
