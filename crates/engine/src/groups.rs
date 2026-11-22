//! Group subjects: history sync carries no names for many groups, so they are
//! fetched from the server (all at once after connecting, and on demand when a
//! message arrives for a still-unnamed group).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::Instant;
use whatsapp_rust::prelude::{Client, Jid};
use whatsapp_rust::wacore::stanza::groups::{GroupNotificationAction, GroupParticipantInfo, MembershipRequestMethod};
use whatsapp_rust::wacore::types::events::GroupUpdate;
use wa_store::{ChatKind, MessageKind, NewMessage, Store};

use crate::api::UiEvent;
use crate::events::Shared;
use crate::jid::redact;
use crate::media::lock;

/// Delay before the bulk fetch so it does not compete with the login burst.
const INITIAL_FETCH_DELAY: Duration = Duration::from_secs(5);
/// How long a failed or empty lookup suppresses further attempts.
const NEGATIVE_TTL: Duration = Duration::from_secs(10 * 60);
/// Concurrent `get_metadata` IQs.
const METADATA_PERMITS: usize = 2;
/// Minimum spacing between the starts of two `get_metadata` IQs.
const METADATA_PACE: Duration = Duration::from_millis(300);

/// Reserves the next request slot: returns when this request may start and the
/// earliest start of the one after it.
fn next_slot(now: Instant, earliest: Instant) -> (Instant, Instant) {
    let start = now.max(earliest);
    (start, start + METADATA_PACE)
}

#[derive(Default)]
struct State {
    in_flight: HashSet<String>,
    negative: HashMap<String, Instant>,
    /// Groups known to have a name in the store.
    named: HashSet<String>,
    /// Join-request notices: (group, text hash) -> (first, latest) timestamp of the run.
    request_runs: HashMap<String, (i64, i64)>,
}

/// Per-generation bookkeeping of subject lookups.
pub struct GroupNames {
    initial_started: AtomicBool,
    state: Mutex<State>,
    metadata_permits: Semaphore,
    metadata_pace: tokio::sync::Mutex<Option<Instant>>,
    /// Our own PN / LID (non-AD, as strings); lets notices say "You".
    own: Mutex<Vec<String>>,
}

impl Default for GroupNames {
    fn default() -> Self {
        GroupNames {
            initial_started: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            metadata_permits: Semaphore::new(METADATA_PERMITS),
            metadata_pace: tokio::sync::Mutex::new(None),
            own: Mutex::new(Vec::new()),
        }
    }
}

impl GroupNames {
    /// Forgets every cached lookup (logout: the next account must not inherit
    /// another one's group state).
    pub fn clear(&self) {
        *lock(&self.state) = State::default();
        lock(&self.own).clear();
        self.initial_started.store(false, Ordering::Release);
    }

    /// Records our own identities (PN and LID) for the "You" wording of
    /// group notices; call it once the session knows them.
    pub fn set_own(&self, ids: impl IntoIterator<Item = String>) {
        *lock(&self.own) = ids.into_iter().collect();
    }

    /// Waits for a permit and the next pacing slot of a metadata IQ.
    pub(crate) async fn metadata_slot(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        let permit = self.metadata_permits.acquire().await.ok()?;
        let wait = {
            let mut earliest = self.metadata_pace.lock().await;
            let now = Instant::now();
            let (start, next) = next_slot(now, earliest.unwrap_or(now));
            *earliest = Some(next);
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Some(permit)
    }

    /// Cheap pre-check that avoids touching the store.
    fn skip(&self, jid: &str) -> bool {
        let st = lock(&self.state);
        st.named.contains(jid)
            || st.in_flight.contains(jid)
            || st.negative.get(jid).is_some_and(|until| *until > Instant::now())
    }

    fn mark_named(&self, jid: &str) {
        let mut st = lock(&self.state);
        st.named.insert(jid.to_string());
        st.negative.remove(jid);
    }

    /// Returns `false` if a lookup for `jid` is already running or recently failed.
    fn claim(&self, jid: &str) -> bool {
        let mut st = lock(&self.state);
        if st.in_flight.contains(jid) || st.negative.get(jid).is_some_and(|u| *u > Instant::now())
        {
            return false;
        }
        st.in_flight.insert(jid.to_string());
        true
    }

    fn release(&self, jid: &str, failed: bool) {
        let mut st = lock(&self.state);
        st.in_flight.remove(jid);
        if failed {
            st.negative.insert(jid.to_string(), Instant::now() + NEGATIVE_TTL);
        }
    }
}

pub(crate) fn store_subject(sh: &Shared, jid: &str, subject: &str) {
    if subject.is_empty() {
        return;
    }
    match sh.store.upsert_chat(jid, ChatKind::Group, Some(subject)) {
        Ok(()) => sh.groups.mark_named(jid),
        Err(e) => log::error!("storing subject of {} failed: {e}", redact(jid)),
    }
}

/// Fetches every group we participate in, once per bot generation.
pub fn spawn_initial_fetch(sh: Shared, client: Arc<Client>) {
    if sh.groups.initial_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let tasks = sh.tasks.clone();
    tasks.spawn(async move {
        tokio::time::sleep(INITIAL_FETCH_DELAY).await;
        match client.groups().get_participating().await {
            Ok(groups) => {
                log::info!("fetched subjects of {} groups", groups.len());
                for (jid, meta) in &groups {
                    store_subject(&sh, &jid.to_string(), &meta.subject);
                }
                sh.emit(UiEvent::ChatsChanged);
            }
            Err(e) => {
                crate::jid::warn_lib_error("fetching group list failed", &e);
                // Let the next Connected try again.
                sh.groups.initial_started.store(false, Ordering::Release);
            }
        }
    });
}

/// Called after a message was stored for `chat`; fetches the subject if the
/// group is still unnamed.
pub fn ensure_name(sh: &Shared, client: &Arc<Client>, chat: &str) {
    if !chat.ends_with("@g.us") || sh.groups.skip(chat) {
        return;
    }
    match sh.store.chats_missing_name(ChatKind::Group) {
        Ok(missing) if missing.iter().any(|j| j == chat) => {}
        Ok(_) => {
            sh.groups.mark_named(chat);
            return;
        }
        Err(e) => {
            log::error!("chats_missing_name failed: {e}");
            return;
        }
    }
    let Ok(jid) = chat.parse::<Jid>() else {
        return;
    };
    if !sh.groups.claim(chat) {
        return;
    }
    let (sh, client, chat) = (sh.clone(), client.clone(), chat.to_string());
    let tasks = sh.tasks.clone();
    tasks.spawn(async move {
        let Some(_slot) = sh.groups.metadata_slot().await else {
            sh.groups.release(&chat, true);
            return;
        };
        let failed = match client.groups().get_metadata(&jid).await {
            Ok(meta) if !meta.subject.is_empty() => {
                store_subject(&sh, &chat, &meta.subject);
                sh.emit(UiEvent::ChatsChanged);
                false
            }
            Ok(_) => true,
            Err(e) => {
                log::debug!("group metadata for {} failed: {e}", redact(&chat));
                true
            }
        };
        sh.groups.release(&chat, failed);
    });
}

/// Notices list at most this many names before collapsing into "N others".
const MAX_LISTED: usize = 5;
/// Names shown ahead of "and N others" once the list is collapsed.
const SHOWN_WHEN_COLLAPSED: usize = 3;

/// A person named in a group notification, already resolved.
#[derive(Debug, Clone, PartialEq)]
struct Who {
    name: String,
    me: bool,
    ids: Vec<String>,
}

impl Who {
    fn same_person(&self, other: &Who) -> bool {
        self.ids.iter().any(|i| other.ids.contains(i))
    }

    /// "You" / "you" for ourselves, the display name otherwise.
    fn label(&self, sentence_start: bool) -> String {
        match (self.me, sentence_start) {
            (true, true) => "You".to_string(),
            (true, false) => "you".to_string(),
            _ => self.name.clone(),
        }
    }
}

/// "A", "A and B", "A, B and C"; more than five collapse to "A, B, C and 4 others".
fn join_names(people: &[Who], sentence_start: bool) -> String {
    let labels: Vec<String> = people
        .iter()
        .enumerate()
        .map(|(i, w)| w.label(sentence_start && i == 0))
        .collect();
    match labels.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        many if many.len() > MAX_LISTED => {
            let (shown, rest) = many.split_at(SHOWN_WHEN_COLLAPSED);
            format!("{} and {} others", shown.join(", "), rest.len())
        }
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// "was"/"were" for the passive forms.
fn be(people: &[Who]) -> &'static str {
    if people.len() == 1 && !people[0].me { "was" } else { "were" }
}

/// The English notice for one group action; `None` for actions that show
/// nothing. `targets` are the resolved participants of the action, in order.
fn notice_text(action: &GroupNotificationAction, actor: Option<&Who>, targets: &[Who]) -> Option<String> {
    let by = |verb: &str| match actor {
        Some(a) => format!("{} {verb}", a.label(true)),
        None => format!("Someone {verb}"),
    };
    let text = match action {
        GroupNotificationAction::Add { reason, .. } => {
            if targets.is_empty() {
                return None;
            }
            let invited = reason.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("invite"));
            let self_join = actor.is_some_and(|a| targets.iter().all(|t| t.same_person(a)));
            if invited || self_join {
                format!("{} joined using this group's invite link", join_names(targets, true))
            } else if let Some(a) = actor {
                format!("{} added {}", a.label(true), join_names(targets, false))
            } else {
                format!("{} {} added", join_names(targets, true), be(targets))
            }
        }
        GroupNotificationAction::Remove { .. } => {
            if targets.is_empty() {
                return None;
            }
            match actor {
                Some(a) if targets.iter().all(|t| t.same_person(a)) => {
                    format!("{} left", join_names(targets, true))
                }
                Some(a) => format!("{} removed {}", a.label(true), join_names(targets, false)),
                None => format!("{} {} removed", join_names(targets, true), be(targets)),
            }
        }
        GroupNotificationAction::Promote { .. } => match (targets, actor) {
            ([], _) => return None,
            ([t], _) if t.me => "You're now an admin".to_string(),
            ([_], Some(a)) => format!("{} made {} an admin", a.label(true), join_names(targets, false)),
            (_, Some(a)) => format!("{} made {} admins", a.label(true), join_names(targets, false)),
            ([_], None) => format!("{} is now an admin", join_names(targets, true)),
            (_, None) => format!("{} are now admins", join_names(targets, true)),
        },
        GroupNotificationAction::Demote { .. } => match targets {
            [] => return None,
            [t] if t.me => "You're no longer an admin".to_string(),
            [_] => format!("{} is no longer an admin", join_names(targets, true)),
            _ => format!("{} are no longer admins", join_names(targets, true)),
        },
        GroupNotificationAction::Subject { subject, .. } => {
            if subject.is_empty() {
                return None;
            }
            format!("{} \"{subject}\"", by("changed the group name to"))
        }
        GroupNotificationAction::Description { .. } => by("changed the group description"),
        GroupNotificationAction::Locked { .. } => {
            by("changed this group's settings to allow only admins to edit this group's info")
        }
        GroupNotificationAction::Unlocked => {
            by("changed this group's settings to allow all members to edit this group's info")
        }
        GroupNotificationAction::Announce => {
            by("changed this group's settings to allow only admins to send messages")
        }
        GroupNotificationAction::NotAnnounce => {
            by("changed this group's settings to allow all members to send messages")
        }
        GroupNotificationAction::MembershipApprovalMode { enabled } => {
            let state = if *enabled { "on" } else { "off" };
            by(&format!("turned {state} admin approval to join this group"))
        }
        GroupNotificationAction::RevokeInvite => by("reset this group's invite link"),
        // The people asking are the targets; a plain request has only the actor.
        GroupNotificationAction::MembershipApprovalRequest { .. } => match actor {
            Some(a) => format!("{} asked to join", a.label(true)),
            None => return None,
        },
        GroupNotificationAction::CreatedMembershipRequests { request_method, .. } => match (actor, targets) {
            (None, []) => return None,
            (Some(a), []) => format!("{} asked to join", a.label(true)),
            (Some(a), _)
                if *request_method == MembershipRequestMethod::NonAdminAdd
                    && !targets.iter().all(|t| t.same_person(a)) =>
            {
                format!("{} asked to add {}", a.label(true), join_names(targets, false))
            }
            (_, _) => format!("{} asked to join", join_names(targets, true)),
        },
        // A cancelled or rejected request is not worth a line in the chat; the
        // pending count is refreshed instead (see `on_group_update`).
        _ => return None,
    };
    Some(text)
}

/// The system-notice row (kind Other). Its id makes a replayed notification
/// collapse into the one row.
fn notice_row(group: &str, id: String, ts: i64, text: String) -> NewMessage {
    NewMessage {
        chat_jid: group.to_string(),
        id,
        sender_jid: group.to_string(),
        from_me: false,
        ts,
        kind: MessageKind::Other,
        text: Some(text),
        ..Default::default()
    }
}

/// Stores a notice without counting it as unread; `false` when it was already there.
fn insert_notice(store: &Store, row: &NewMessage) -> Result<bool, wa_store::StoreError> {
    if store.chat(&row.chat_jid)?.is_none() {
        store.upsert_chat(&row.chat_jid, ChatKind::Group, None)?;
    }
    store.insert_message(row, false)
}

fn non_ad(jid: &Jid) -> String {
    jid.to_non_ad().to_string()
}

/// Address-book name, then push name, then the stored phone.
fn contact_name(store: &Store, ids: &[String]) -> Option<String> {
    ids.iter().find_map(|id| {
        let c = store.contact(id).ok().flatten()?;
        c.full_name
            .filter(|n| !n.is_empty())
            .or(c.push_name.filter(|n| !n.is_empty()))
            .or(c.phone)
    })
}

fn resolve(sh: &Shared, own: &[String], jids: [Option<&Jid>; 3], label: Option<&str>) -> Who {
    let mut ids: Vec<String> = Vec::new();
    for j in jids.into_iter().flatten() {
        let id = non_ad(j);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let me = ids.iter().any(|i| own.contains(i));
    let name = contact_name(&sh.store, &ids)
        .or_else(|| label.filter(|l| !l.is_empty()).map(str::to_string))
        .or_else(|| {
            ids.iter()
                .find_map(|i| i.strip_suffix("@s.whatsapp.net"))
                .map(|user| format!("+{user}"))
        })
        .unwrap_or_else(|| "Someone".to_string());
    Who { name, me, ids }
}

fn participants_of(action: &GroupNotificationAction) -> &[GroupParticipantInfo] {
    match action {
        GroupNotificationAction::Add { participants, .. }
        | GroupNotificationAction::Remove { participants, .. }
        | GroupNotificationAction::Promote { participants }
        | GroupNotificationAction::Demote { participants } => participants,
        GroupNotificationAction::CreatedMembershipRequests { requests, .. } => requests,
        _ => &[],
    }
}

fn is_request_action(action: &GroupNotificationAction) -> bool {
    matches!(
        action,
        GroupNotificationAction::MembershipApprovalRequest { .. } | GroupNotificationAction::CreatedMembershipRequests { .. }
    )
}

/// Repeats of the same request text within this many seconds of the previous
/// one belong to the same notice.
const REQUEST_WINDOW_SECS: i64 = 5 * 60;

/// The server may announce one join request in two notifications; both give
/// the same text within moments. The id is a stable hash of the text plus the
/// timestamp that started a sliding run: a repeat within the window of the
/// run's last-seen timestamp keeps the run's id (frozen at the first-seen
/// timestamp, even for out-of-order repeats), a later one starts a new run.
/// Runs idle for longer than the window are dropped so the map stays small.
fn request_notice_id(runs: &mut HashMap<String, (i64, i64)>, group: &str, text: &str, ts: i64) -> String {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    h.update(group.as_bytes());
    h.update([0]);
    h.update(text.as_bytes());
    let digest: String = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    runs.retain(|_, r| ts.saturating_sub(r.1) <= REQUEST_WINDOW_SECS);
    let run = runs.entry(digest.clone()).or_insert((ts, ts));
    if (ts - run.1).abs() > REQUEST_WINDOW_SECS {
        *run = (ts, ts);
    } else {
        run.1 = run.1.max(ts);
    }
    format!("GRP-REQ-{digest}-{}", run.0)
}

/// Writes the in-chat notice for a group change, if it has one.
fn write_notice(sh: &Shared, u: &GroupUpdate) {
    let own = lock(&sh.groups.own).clone();
    let actor = u
        .participant
        .as_ref()
        .map(|p| resolve(sh, &own, [Some(p), u.participant_pn.as_ref(), None], u.notify.as_deref()));
    let targets: Vec<Who> = participants_of(&u.action)
        .iter()
        .map(|p| resolve(sh, &own, [Some(&p.jid), p.phone_number.as_ref(), p.lid.as_ref()], p.display_name.as_deref()))
        .collect();
    let Some(text) = notice_text(&u.action, actor.as_ref(), &targets) else {
        return;
    };
    let ts = u.timestamp.timestamp();
    let id = if is_request_action(&u.action) {
        request_notice_id(&mut lock(&sh.groups.state).request_runs, &u.group_jid.to_string(), &text, ts)
    } else {
        match u.notification_id.as_deref().filter(|n| !n.is_empty()) {
            Some(n) => format!("GRP-{n}-{}", u.action_index),
            None => format!("GRP-{ts}-{}", u.action_index),
        }
    };
    let chat = u.group_jid.to_string();
    match insert_notice(&sh.store, &notice_row(&chat, id, ts, text)) {
        Ok(true) => {
            sh.emit(UiEvent::MessagesChanged { chat_jid: chat });
            sh.emit(UiEvent::ChatsChanged);
        }
        Ok(false) => {}
        Err(e) => log::error!("storing a group notice failed: {e}"),
    }
}

/// Applies a group change notification: cache invalidation, new subject, and
/// the in-chat notice.
pub fn on_group_update(sh: &Shared, u: &GroupUpdate) {
    sh.info.invalidate(&u.group_jid.to_string());
    if let GroupNotificationAction::Subject { subject, .. } = &u.action {
        store_subject(sh, &u.group_jid.to_string(), subject);
        sh.emit(UiEvent::ChatsChanged);
    }
    write_notice(sh, u);
    if matches!(
        u.action,
        GroupNotificationAction::MembershipApprovalRequest { .. }
            | GroupNotificationAction::CreatedMembershipRequests { .. }
            | GroupNotificationAction::RevokedMembershipRequests { .. }
            | GroupNotificationAction::MembershipApprovalMode { .. }
    ) {
        sh.emit(UiEvent::MembershipRequestsChanged { group_jid: u.group_jid.to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn claims_dedupe_and_negative_results_expire() {
        let g = GroupNames::default();
        assert!(g.claim("g@g.us"));
        assert!(!g.claim("g@g.us"), "in flight");
        g.release("g@g.us", true);
        assert!(g.skip("g@g.us"));
        assert!(!g.claim("g@g.us"), "negative cached");
        tokio::time::sleep(NEGATIVE_TTL + Duration::from_secs(1)).await;
        assert!(!g.skip("g@g.us"));
        assert!(g.claim("g@g.us"));
        g.release("g@g.us", false);
        g.mark_named("g@g.us");
        assert!(g.skip("g@g.us"));
        g.clear();
        assert!(!g.skip("g@g.us"), "clear forgets named groups");
    }

    #[test]
    fn metadata_requests_are_paced() {
        let t0 = Instant::now();
        let (s1, e1) = next_slot(t0, t0);
        assert_eq!(s1, t0);
        assert_eq!(e1 - s1, METADATA_PACE);
        let (s2, _) = next_slot(t0, e1);
        assert_eq!(s2, e1);
        let later = e1 + Duration::from_secs(5);
        assert_eq!(next_slot(later, e1).0, later);
    }

    fn who(name: &str, me: bool) -> Who {
        Who { name: name.into(), me, ids: vec![format!("{name}@s.whatsapp.net")] }
    }

    fn add(reason: Option<&str>) -> GroupNotificationAction {
        GroupNotificationAction::Add { participants: vec![], reason: reason.map(str::to_string) }
    }

    fn remove() -> GroupNotificationAction {
        GroupNotificationAction::Remove { participants: vec![], reason: None }
    }

    #[test]
    fn participant_notices() {
        let (ali, sara, bob, me) = (who("Ali", false), who("Sara", false), who("Bob", false), who("Me", true));
        let t = notice_text;
        assert_eq!(t(&add(None), Some(&ali), &[sara.clone(), bob.clone()]).unwrap(), "Ali added Sara and Bob");
        assert_eq!(t(&add(None), Some(&me), &[sara.clone()]).unwrap(), "You added Sara");
        assert_eq!(t(&add(None), Some(&ali), &[me.clone()]).unwrap(), "Ali added you");
        assert_eq!(t(&add(None), None, &[sara.clone()]).unwrap(), "Sara was added");
        assert_eq!(
            t(&add(Some("invite")), Some(&ali), &[sara.clone()]).unwrap(),
            "Sara joined using this group's invite link"
        );
        assert_eq!(
            t(&add(None), Some(&sara), &[sara.clone()]).unwrap(),
            "Sara joined using this group's invite link"
        );
        assert_eq!(t(&remove(), Some(&ali), &[ali.clone()]).unwrap(), "Ali left");
        assert_eq!(t(&remove(), Some(&me), &[me.clone()]).unwrap(), "You left");
        assert_eq!(t(&remove(), Some(&ali), &[sara.clone()]).unwrap(), "Ali removed Sara");
        assert_eq!(t(&remove(), None, &[me.clone()]).unwrap(), "You were removed");
        assert_eq!(t(&add(None), Some(&ali), &[]), None);
    }

    #[test]
    fn admin_notices() {
        let (ali, sara, me) = (who("Ali", false), who("Sara", false), who("Me", true));
        let promote = GroupNotificationAction::Promote { participants: vec![] };
        let demote = GroupNotificationAction::Demote { participants: vec![] };
        assert_eq!(notice_text(&promote, Some(&ali), &[sara.clone()]).unwrap(), "Ali made Sara an admin");
        assert_eq!(notice_text(&promote, Some(&ali), &[me.clone()]).unwrap(), "You're now an admin");
        assert_eq!(notice_text(&demote, Some(&ali), &[sara.clone()]).unwrap(), "Sara is no longer an admin");
        assert_eq!(notice_text(&demote, None, &[me.clone()]).unwrap(), "You're no longer an admin");
    }

    #[test]
    fn setting_notices() {
        let (ali, me) = (who("Ali", false), who("Me", true));
        let subject = GroupNotificationAction::Subject { subject: "Trip".into(), subject_owner: None, subject_time: None };
        assert_eq!(notice_text(&subject, Some(&ali), &[]).unwrap(), "Ali changed the group name to \"Trip\"");
        assert_eq!(notice_text(&subject, Some(&me), &[]).unwrap(), "You changed the group name to \"Trip\"");
        assert_eq!(
            notice_text(&GroupNotificationAction::Announce, Some(&ali), &[]).unwrap(),
            "Ali changed this group's settings to allow only admins to send messages"
        );
        assert_eq!(
            notice_text(&GroupNotificationAction::Unlocked, Some(&ali), &[]).unwrap(),
            "Ali changed this group's settings to allow all members to edit this group's info"
        );
        assert_eq!(
            notice_text(&GroupNotificationAction::MembershipApprovalMode { enabled: true }, Some(&ali), &[]).unwrap(),
            "Ali turned on admin approval to join this group"
        );
        assert_eq!(
            notice_text(&GroupNotificationAction::RevokeInvite, Some(&ali), &[]).unwrap(),
            "Ali reset this group's invite link"
        );
        assert_eq!(notice_text(&GroupNotificationAction::NoFrequentlyForwarded, Some(&ali), &[]), None);
    }

    #[test]
    fn join_request_notices() {
        let (ali, sara, bob, me) = (who("Ali", false), who("Sara", false), who("Bob", false), who("Me", true));
        let plain = GroupNotificationAction::MembershipApprovalRequest {
            request_method: MembershipRequestMethod::InviteLink,
            parent_group_jid: None,
        };
        assert_eq!(notice_text(&plain, Some(&sara), &[]).unwrap(), "Sara asked to join");
        assert_eq!(notice_text(&plain, None, &[]), None);
        let created = |method| GroupNotificationAction::CreatedMembershipRequests {
            request_method: method,
            parent_group_jid: None,
            requests: vec![],
        };
        let link = created(MembershipRequestMethod::InviteLink);
        assert_eq!(notice_text(&link, Some(&sara), &[sara.clone()]).unwrap(), "Sara asked to join");
        assert_eq!(notice_text(&link, None, &[sara.clone(), bob.clone()]).unwrap(), "Sara and Bob asked to join");
        assert_eq!(notice_text(&link, Some(&sara), &[]).unwrap(), "Sara asked to join");
        assert_eq!(notice_text(&link, None, &[]), None);
        let added = created(MembershipRequestMethod::NonAdminAdd);
        assert_eq!(notice_text(&added, Some(&ali), &[sara.clone()]).unwrap(), "Ali asked to add Sara");
        assert_eq!(notice_text(&added, Some(&sara), &[sara.clone()]).unwrap(), "Sara asked to join");
        assert_eq!(notice_text(&added, Some(&ali), &[me]).unwrap(), "Ali asked to add you");
        let revoked = GroupNotificationAction::RevokedMembershipRequests { participants: vec![] };
        assert_eq!(notice_text(&revoked, Some(&ali), &[]), None, "cancelled requests make no notice");
    }

    #[test]
    fn duplicate_request_notifications_fold_into_one_row() {
        let mut runs = HashMap::new();
        let a = request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 1000);
        assert_eq!(a, request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 1001));
        assert_ne!(a, request_notice_id(&mut runs, "1@g.us", "Bob asked to join", 1000));
        assert_ne!(a, request_notice_id(&mut runs, "2@g.us", "Sara asked to join", 1000));
        assert!(a.starts_with("GRP-REQ-"));
    }

    #[test]
    fn request_dedupe_slides_across_old_bucket_boundaries() {
        let mut runs = HashMap::new();
        // 119 and 121 fell into different fixed 120 s buckets before.
        let a = request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 119);
        assert_eq!(a, request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 121));
        // Each repeat extends the window.
        assert_eq!(a, request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 121 + 290));
        assert_eq!(a, request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 121 + 290 + 290));
        // A gap longer than the window starts a new notice.
        let b = request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 121 + 290 + 290 + 301);
        assert_ne!(a, b);
    }

    #[test]
    fn out_of_order_duplicate_keeps_the_run_id() {
        let mut runs = HashMap::new();
        let a = request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 1000);
        assert_eq!(a, request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 1100));
        assert_eq!(a, request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 990));
        assert!(a.ends_with("-1000"));
    }

    #[test]
    fn idle_runs_are_pruned() {
        let mut runs = HashMap::new();
        for i in 0..100 {
            request_notice_id(&mut runs, "1@g.us", &format!("user {i} asked to join"), i * (REQUEST_WINDOW_SECS + 1));
        }
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn request_notice_hash_is_fixed() {
        let mut runs = HashMap::new();
        let id = request_notice_id(&mut runs, "1@g.us", "Sara asked to join", 7);
        assert_eq!(id.len(), "GRP-REQ-".len() + 16 + "-7".len());
        assert!(id.ends_with("-7"));
    }

    #[test]
    fn long_lists_collapse() {
        let people: Vec<Who> = (0..7).map(|i| who(&format!("P{i}"), false)).collect();
        assert_eq!(join_names(&people[..5], true), "P0, P1, P2, P3 and P4");
        assert_eq!(join_names(&people, true), "P0, P1, P2 and 4 others");
    }

    #[test]
    fn notice_insertion_is_idempotent_and_not_unread() {
        let store = Store::open_in_memory().unwrap();
        let g = "1@g.us";
        let row = notice_row(g, "GRP-N1-0".into(), 100, "Ali left".into());
        assert!(insert_notice(&store, &row).unwrap());
        assert!(!insert_notice(&store, &row).unwrap(), "replay is ignored");
        let stored = store.message(g, "GRP-N1-0").unwrap().expect("stored");
        assert_eq!(stored.kind, MessageKind::Other);
        assert_eq!(store.chat(g).unwrap().unwrap().unread, 0);
    }
}
