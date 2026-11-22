//! Voting on and creating polls (Command::VotePoll / Command::CreatePoll).
//!
//! Voter identity: a vote is stored under `own_jid` (our phone-number JID,
//! the form `sender_jid` of our own messages has). Echoes of our own votes
//! from another device are stored by `events::apply_poll_vote` under the same
//! key (`from_me` messages always use `own_jid`), so an optimistic vote and its
//! echo replace each other instead of counting twice. The wire-level voter
//! JID (which keys the vote encryption) is chosen by the library from the
//! poll creator's address namespace, so [`creator_form`] hands it the creator
//! in the namespace the poll was really sent in, not the store's PN form.

use std::sync::Arc;

use whatsapp_rust::buffa::MessageField;
use whatsapp_rust::prelude::{Client, Jid, wa};
use whatsapp_rust::wacore::reporting_token::generate_message_secret;
use wa_store::Store;

use crate::api::UiEvent;
use whatsapp_rust::wacore::types::message::AddressingMode;
use whatsapp_rust::wacore_binary::JidExt as _;

use crate::commands::{NOT_LOGGED_IN, deliver, parse_jid, reject, sender_forms, store_pending};
use crate::convert;
use crate::events::{Shared, load_poll_secrets, now_secs, own_jid, record_poll};
use crate::info::{self, own_jids};
use crate::jid::{redact, warn_lib_error};

const MAX_QUESTION_CHARS: usize = 255;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 12;

/// The options to vote for, in poll order and without repeats. `selectable`
/// is the poll's limit (0 = any number).
fn resolve_selection(poll_options: &[String], selectable: u32, requested: &[String]) -> Result<Vec<String>, String> {
    if requested.iter().any(|r| !poll_options.contains(r)) {
        return Err("That option is not part of this poll".to_string());
    }
    let chosen: Vec<String> = poll_options.iter().filter(|o| requested.contains(o)).cloned().collect();
    if selectable > 0 && chosen.len() > selectable as usize {
        return Err(if selectable == 1 {
            "This poll allows only one answer".to_string()
        } else {
            format!("This poll allows at most {selectable} answers")
        });
    }
    Ok(chosen)
}

/// Writes our vote and returns what it replaced. The timestamp is never older
/// than the replaced vote, which the store would otherwise ignore.
fn optimistic_vote(
    store: &Store,
    chat: &str,
    poll_id: &str,
    voter: &str,
    names: &[String],
    now: i64,
) -> Result<(Option<(Vec<String>, i64)>, i64), String> {
    let previous = store.poll_vote_of(chat, poll_id, voter).map_err(|e| e.to_string())?;
    let ts = previous.as_ref().map_or(now, |(_, t)| now.max(*t));
    match store.upsert_poll_vote(chat, poll_id, voter, names, ts, true) {
        Ok(true) => Ok((previous, ts)),
        Ok(false) => Err("vote rejected by the poll's limit".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn revert_vote(
    store: &Store,
    chat: &str,
    poll_id: &str,
    voter: &str,
    written_ts: i64,
    previous: Option<&(Vec<String>, i64)>,
) -> bool {
    match store.restore_poll_vote(chat, poll_id, voter, written_ts, previous, true) {
        Ok(changed) => changed,
        Err(e) => {
            log::error!("reverting the poll vote failed: {e}");
            false
        }
    }
}

/// The poll creator's address in the namespace the poll was sent in. The
/// library derives the vote key (and our voter JID) from the namespace of the
/// creator JID it is given, and other clients derive it from the poll's own
/// namespace, so the two have to agree.
///
/// - LID-addressed group: the creator's LID (`lid`); when it is unknown we fall
///   back to the phone form and report that through the second value.
/// - PN-addressed group: the creator's phone form (`pn`).
/// - Group of unknown mode, or a one-to-one chat (the library exposes no
///   addressing signal for those): the stored JID unchanged, i.e. the phone
///   form whenever the store knew the mapping, the LID otherwise.
///
/// Returns the JID and whether a needed LID was missing.
fn creator_form(
    group: bool,
    mode: Option<AddressingMode>,
    stored: &Jid,
    pn: Option<Jid>,
    lid: Option<Jid>,
) -> (Jid, bool) {
    match (group, mode) {
        (true, Some(AddressingMode::Lid)) => match lid {
            Some(lid) => (lid, false),
            None => (pn.unwrap_or_else(|| stored.to_non_ad()), true),
        },
        (true, Some(AddressingMode::Pn)) => (pn.unwrap_or_else(|| stored.to_non_ad()), false),
        _ => (stored.to_non_ad(), false),
    }
}

async fn wire_creator(sh: &Shared, client: &Client, chat: &Jid, stored: &Jid) -> Jid {
    let group = chat.is_group();
    if !group {
        return creator_form(false, None, stored, None, None).0;
    }
    let mode = info::group_addressing(sh, client, chat).await;
    let base = stored.to_non_ad();
    let (mut pn, mut lid) = sender_forms(client, &base).await;
    // We are the creator: our own LID is known without the mapping.
    if own_jids(client).contains(&base.to_string()) {
        lid = client.lid().map(|j| j.to_non_ad());
        pn = pn.or_else(|| own_jid(client).and_then(|s| s.parse::<Jid>().ok()));
    }
    let (creator, missing_lid) = creator_form(true, mode, stored, pn, lid);
    if missing_lid {
        log::debug!("poll creator LID unknown in {}, voting with the phone form", redact(&chat.to_string()));
    }
    creator
}

pub async fn vote(sh: &Shared, client: &Arc<Client>, chat_jid: &str, poll_id: &str, options: &[String]) {
    let Some(chat) = parse_jid(sh, chat_jid) else { return };
    let Some(voter) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    let secrets = match load_poll_secrets(&sh.store, chat_jid, poll_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            reject(sh, "This poll can't be voted on from here".to_string());
            return;
        }
        Err(e) => {
            log::error!("loading poll for voting failed: {e}");
            reject(sh, "Couldn't send your vote".to_string());
            return;
        }
    };
    let selectable = match sh.store.poll_for(chat_jid, poll_id) {
        Ok(Some(view)) => view.selectable_count,
        Ok(None) => {
            reject(sh, "This poll can't be voted on from here".to_string());
            return;
        }
        Err(e) => {
            log::error!("loading poll for voting failed: {e}");
            reject(sh, "Couldn't send your vote".to_string());
            return;
        }
    };
    let names = match resolve_selection(&secrets.options, selectable, options) {
        Ok(n) => n,
        Err(message) => {
            reject(sh, message);
            return;
        }
    };
    let Ok(stored_creator) = secrets.creator.parse::<Jid>() else {
        log::warn!("poll creator address unparsable in {}", redact(chat_jid));
        reject(sh, "This poll can't be voted on from here".to_string());
        return;
    };
    let creator = wire_creator(sh, client, &chat, &stored_creator).await;

    let (previous, written_ts) = match optimistic_vote(&sh.store, chat_jid, poll_id, &voter, &names, now_secs()) {
        Ok(r) => r,
        Err(e) => {
            log::error!("storing own poll vote failed: {e}");
            reject(sh, "Couldn't send your vote".to_string());
            return;
        }
    };
    sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });

    // An empty selection is a withdrawal: the library encrypts the empty
    // option list like WA Web does.
    let result = client.polls().vote(chat, poll_id, &creator, &secrets.secret, &names).await;
    if let Err(e) = result {
        warn_lib_error(&format!("sending a poll vote in {} failed", redact(chat_jid)), &e);
        revert_vote(&sh.store, chat_jid, poll_id, &voter, written_ts, previous.as_ref());
        sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
        reject(sh, "Couldn't send your vote".to_string());
    }
}

/// A validated poll definition: trimmed question and options.
fn validate_new_poll(question: &str, options: &[String]) -> Result<(String, Vec<String>), String> {
    let question = question.trim();
    if question.is_empty() {
        return Err("A poll needs a question".to_string());
    }
    if question.chars().count() > MAX_QUESTION_CHARS {
        return Err(format!("The question can have at most {MAX_QUESTION_CHARS} characters"));
    }
    let options: Vec<String> = options.iter().map(|o| o.trim().to_string()).collect();
    if options.iter().any(String::is_empty) {
        return Err("Poll options can't be empty".to_string());
    }
    if options.len() < MIN_OPTIONS {
        return Err(format!("A poll needs at least {MIN_OPTIONS} options"));
    }
    if options.len() > MAX_OPTIONS {
        return Err(format!("A poll can have at most {MAX_OPTIONS} options"));
    }
    let mut seen = std::collections::HashSet::new();
    if !options.iter().all(|o| seen.insert(o.to_lowercase())) {
        return Err("Poll options must be different from each other".to_string());
    }
    Ok((question.to_string(), options))
}

/// The poll message as WA Web sends it: V3 for a single answer, V1 with
/// selectable count 0 (any number) for several, plus the vote secret.
///
/// A quiz (`correct` = the right option's name) is always a single-answer V3
/// poll with `pollType` QUIZ and `correctAnswer` carrying the name and its
/// lowercase-hex SHA-256, exactly what the library's `create_quiz` builds.
fn build_poll_message(question: &str, options: &[String], multi: bool, secret: [u8; 32], correct: Option<&str>) -> wa::Message {
    let multi = multi && correct.is_none();
    let (poll_type, correct_answer) = match correct {
        Some(name) => (
            Some(wa::message::PollType::QUIZ),
            MessageField::some(wa::message::poll_creation_message::Option {
                option_name: Some(name.to_string()),
                option_hash: Some(convert::quiz_option_hash(name)),
            }),
        ),
        None => (None, MessageField::none()),
    };
    let poll = wa::message::PollCreationMessage {
        name: Some(question.to_string()),
        options: options
            .iter()
            .map(|name| wa::message::poll_creation_message::Option {
                option_name: Some(name.clone()),
                option_hash: None,
            })
            .collect(),
        selectable_options_count: Some(if multi { 0 } else { 1 }),
        poll_content_type: Some(wa::message::PollContentType::TEXT),
        poll_type,
        correct_answer,
        ..Default::default()
    };
    let mut message = if multi {
        wa::Message {
            poll_creation_message: MessageField::some(poll),
            ..Default::default()
        }
    } else {
        wa::Message {
            poll_creation_message_v3: MessageField::some(poll),
            ..Default::default()
        }
    };
    message.message_context_info = MessageField::some(wa::MessageContextInfo {
        message_secret: Some(secret.to_vec()),
        ..Default::default()
    });
    message
}

pub async fn create(
    sh: &Shared,
    client: &Arc<Client>,
    chat_jid: &str,
    question: &str,
    options: &[String],
    multi: bool,
    quiz_correct: Option<usize>,
) {
    let (question, options) = match validate_new_poll(question, options) {
        Ok(v) => v,
        Err(message) => {
            reject(sh, message);
            return;
        }
    };
    let correct = match quiz_correct {
        None => None,
        Some(index) => match options.get(index) {
            Some(name) => Some(name.as_str()),
            None => {
                reject(sh, "Pick the correct answer of the quiz".to_string());
                return;
            }
        },
    };
    let Some(chat) = parse_jid(sh, chat_jid) else { return };
    let Some(sender_jid) = own_jid(client) else {
        reject(sh, NOT_LOGGED_IN.to_string());
        return;
    };
    let message = build_poll_message(&question, &options, multi, generate_message_secret(), correct);
    let id = client.generate_message_id();
    let Some(pending) = convert::outgoing(&message, &id, chat_jid, &sender_jid, now_secs()) else {
        log::error!("built poll message is not displayable");
        reject(sh, "Couldn't create the poll".to_string());
        return;
    };
    if !store_pending(sh, &pending) {
        return;
    }
    record_poll(&sh.store, &pending);
    sh.emit(UiEvent::MessagesChanged { chat_jid: chat_jid.to_string() });
    deliver(sh, client, chat, chat_jid, id, message).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_store::MessageKind;

    const CHAT: &str = "123@s.whatsapp.net";
    const ME: &str = "999@s.whatsapp.net";

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn jid(s: &str) -> Jid {
        s.parse().expect("valid jid")
    }

    #[test]
    fn creator_namespace_follows_the_group_addressing_mode() {
        let stored = jid("5511@s.whatsapp.net");
        let (pn, lid) = (Some(stored.clone()), Some(jid("100@lid")));
        let form = |group, mode, pn: Option<Jid>, lid: Option<Jid>| creator_form(group, mode, &stored, pn, lid);

        assert_eq!(form(true, Some(AddressingMode::Lid), pn.clone(), lid.clone()), (jid("100@lid"), false));
        assert_eq!(form(true, Some(AddressingMode::Pn), pn.clone(), lid.clone()), (stored.clone(), false));
        // A LID group whose creator LID is unknown falls back to the phone form and says so.
        assert_eq!(form(true, Some(AddressingMode::Lid), pn.clone(), None), (stored.clone(), true));
        // Unknown mode and DMs keep the stored form.
        assert_eq!(form(true, None, pn.clone(), lid.clone()), (stored.clone(), false));
        assert_eq!(form(false, None, pn.clone(), lid.clone()), (stored.clone(), false));
        assert_eq!(form(false, Some(AddressingMode::Lid), pn, lid), (stored.clone(), false));
    }

    #[test]
    fn creator_form_drops_device_suffixes_and_keeps_a_stored_lid() {
        let stored = jid("100:7@lid");
        assert_eq!(creator_form(false, None, &stored, None, None).0, jid("100@lid"));
        assert_eq!(
            creator_form(true, Some(AddressingMode::Pn), &stored, None, None).0,
            jid("100@lid"),
            "no phone form known: keep what we have"
        );
    }

    #[test]
    fn selection_must_name_poll_options() {
        let opts = names(&["a", "b", "c"]);
        assert!(resolve_selection(&opts, 0, &names(&["a", "x"])).is_err());
        assert!(resolve_selection(&opts, 0, &names(&["A"])).is_err(), "names match exactly");
        assert_eq!(resolve_selection(&opts, 0, &names(&["c", "a"])).unwrap(), ["a", "c"]);
    }

    #[test]
    fn selection_is_deduplicated_and_limited() {
        let opts = names(&["a", "b", "c"]);
        assert_eq!(resolve_selection(&opts, 1, &names(&["b", "b"])).unwrap(), ["b"]);
        assert!(resolve_selection(&opts, 1, &names(&["a", "b"])).is_err());
        assert!(resolve_selection(&opts, 2, &names(&["a", "b", "c"])).is_err());
        assert_eq!(resolve_selection(&opts, 2, &names(&["a", "b"])).unwrap().len(), 2);
        assert_eq!(resolve_selection(&opts, 0, &names(&["a", "b", "c"])).unwrap().len(), 3);
        assert!(resolve_selection(&opts, 1, &[]).unwrap().is_empty(), "withdrawal");
    }

    #[test]
    fn new_poll_validation() {
        let ok = names(&[" Pizza ", "Sushi"]);
        let (q, o) = validate_new_poll("  Lunch? ", &ok).unwrap();
        assert_eq!((q.as_str(), o), ("Lunch?", names(&["Pizza", "Sushi"])));
        assert!(validate_new_poll("  ", &ok).is_err());
        assert!(validate_new_poll(&"q".repeat(256), &ok).is_err());
        assert!(validate_new_poll(&"q".repeat(255), &ok).is_ok());
        assert!(validate_new_poll("Q", &names(&["only"])).is_err());
        assert!(validate_new_poll("Q", &names(&["a", " "])).is_err());
        assert!(validate_new_poll("Q", &names(&["Tea", "tea "])).is_err(), "case-insensitive");
        let many: Vec<String> = (0..13).map(|i| format!("o{i}")).collect();
        assert!(validate_new_poll("Q", &many).is_err());
        assert!(validate_new_poll("Q", &many[..12]).is_ok());
    }

    #[test]
    fn built_poll_has_secret_and_selectable_count() {
        let opts = names(&["a", "b"]);
        let single = build_poll_message("Q", &opts, false, [7; 32], None);
        assert!(single.poll_creation_message_v3.as_option().is_some());
        assert_eq!(convert::poll_info(&single).unwrap().selectable_count, 1);
        assert_eq!(convert::message_secret(&single), Some(vec![7; 32]));
        let multi = build_poll_message("Q", &opts, true, [7; 32], None);
        assert!(multi.poll_creation_message.as_option().is_some());
        assert_eq!(convert::poll_info(&multi).unwrap().selectable_count, 2, "0 on the wire = any");
    }

    #[test]
    fn quiz_message_carries_type_and_correct_answer_and_parses_back() {
        let opts = names(&["Sydney", "Canberra", "Melbourne"]);
        // `multi` is ignored for a quiz: always one answer.
        let quiz = build_poll_message("Capital?", &opts, true, [5; 32], Some("Canberra"));
        let poll = quiz.poll_creation_message_v3.as_option().expect("V3 poll");
        assert!(matches!(poll.poll_type, Some(wa::message::PollType::QUIZ)));
        assert_eq!(poll.selectable_options_count, Some(1));
        let answer = poll.correct_answer.as_option().expect("correct answer");
        assert_eq!(answer.option_name.as_deref(), Some("Canberra"));
        let want = whatsapp_rust::wacore::poll::compute_option_hash("Canberra");
        let hex: String = want.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(answer.option_hash.as_deref(), Some(hex.as_str()));
        assert_eq!(convert::message_secret(&quiz), Some(vec![5; 32]));

        let info = convert::poll_info(&quiz).unwrap();
        assert_eq!((info.selectable_count, info.correct.as_deref()), (1, Some("Canberra")));
        let plain = build_poll_message("Q", &opts, false, [5; 32], None);
        assert!(plain.poll_creation_message_v3.as_option().unwrap().correct_answer.is_unset());
        assert_eq!(convert::poll_info(&plain).unwrap().correct, None);
    }

    #[test]
    fn quiz_answer_is_stored_with_the_outgoing_poll() {
        let store = Store::open_in_memory().unwrap();
        let opts = names(&["a", "b"]);
        let message = build_poll_message("Q", &opts, false, [9; 32], Some("b"));
        let pending = convert::outgoing(&message, "Q1", CHAT, ME, 100).expect("displayable");
        assert!(store.insert_outgoing(&pending).unwrap());
        record_poll(&store, &pending);
        assert_eq!(store.poll_for(CHAT, "Q1").unwrap().unwrap().correct, Some(1));
    }

    #[test]
    fn stored_outgoing_poll_yields_secrets_for_vote_decryption() {
        let store = Store::open_in_memory().unwrap();
        let opts = names(&["a", "b"]);
        let message = build_poll_message("Q", &opts, false, [9; 32], None);
        let pending = convert::outgoing(&message, "P1", CHAT, ME, 100).expect("displayable");
        assert_eq!(pending.kind, MessageKind::Other);
        assert!(store.insert_outgoing(&pending).unwrap());
        record_poll(&store, &pending);

        let secrets = load_poll_secrets(&store, CHAT, "P1").unwrap().expect("secrets");
        assert_eq!(secrets.creator, ME);
        assert_eq!(secrets.secret, vec![9; 32]);
        assert_eq!(secrets.options, opts);
        assert_eq!(store.poll_for(CHAT, "P1").unwrap().unwrap().selectable_count, 1);
    }

    #[test]
    fn optimistic_vote_and_revert() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_poll(CHAT, "P", "Q", &names(&["a", "b"]), 0).unwrap();
        let tally = |s: &Store| s.poll_for(CHAT, "P").unwrap().unwrap().options;

        let (prev, ts1) = optimistic_vote(&store, CHAT, "P", ME, &names(&["a"]), 10).unwrap();
        assert!(prev.is_none());
        assert_eq!(tally(&store)[0], ("a".to_string(), 1, true));
        assert!(revert_vote(&store, CHAT, "P", ME, ts1, prev.as_ref()));
        assert_eq!(tally(&store)[0].1, 0, "first vote removed");

        let (_, ts1) = optimistic_vote(&store, CHAT, "P", ME, &names(&["a"]), 10).unwrap();
        let (prev, ts2) = optimistic_vote(&store, CHAT, "P", ME, &names(&["b"]), 20).unwrap();
        assert_eq!(prev, Some((names(&["a"]), ts1)));
        assert_eq!(tally(&store)[1].1, 1);
        assert!(revert_vote(&store, CHAT, "P", ME, ts2, prev.as_ref()));
        assert_eq!(tally(&store)[0].1, 1, "previous vote restored");
        assert_eq!(tally(&store)[1].1, 0);

        // Withdrawal, then revert brings the vote back.
        let (prev, ts3) = optimistic_vote(&store, CHAT, "P", ME, &[], 30).unwrap();
        assert_eq!(tally(&store)[0].1, 0);
        assert!(revert_vote(&store, CHAT, "P", ME, ts3, prev.as_ref()));
        assert_eq!(tally(&store)[0].1, 1);
    }

    #[test]
    fn revert_leaves_a_newer_vote_alone() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_poll(CHAT, "P", "Q", &names(&["a", "b"]), 0).unwrap();
        let (prev, ts) = optimistic_vote(&store, CHAT, "P", ME, &names(&["a"]), 10).unwrap();
        // The same account voted from another device meanwhile; no double count.
        store.upsert_poll_vote(CHAT, "P", ME, &names(&["b"]), 15, true).unwrap();
        assert!(!revert_vote(&store, CHAT, "P", ME, ts, prev.as_ref()));
        let opts = store.poll_for(CHAT, "P").unwrap().unwrap().options;
        assert_eq!((opts[0].1, opts[1].1), (0, 1));
    }
}
