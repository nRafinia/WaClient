//! Group creation and administration (Command::CreateGroup and friends).
//!
//! Every handler validates its input first (plain-English `UiEvent::Error` on
//! failure), calls the library, and on success drops the cached info of the
//! group and re-runs [`crate::info::fetch`] so the panel shows the new state.
//! Nothing here logs subjects, descriptions, invite codes or full JIDs.

use std::path::PathBuf;
use std::sync::Arc;

use wa_store::ChatKind;
use whatsapp_rust::prelude::{Client, Jid};
use whatsapp_rust::wacore::types::message::AddressingMode;
use whatsapp_rust::wacore_binary::JidExt as _;
use whatsapp_rust::{
    GroupCreateOptions, GroupDescription, GroupError, GroupMetadata, GroupParticipantOptions, GroupSubject,
    JoinGroupResult, MembershipApprovalMode, MembershipRequest as LibRequest, ParticipantChangeResponse,
    PreviousDescription,
};

use crate::account::{read_picture_file, reencode_profile_picture};
use crate::api::{
    GroupSetting, InvitePreview, MemberAction, MemberChangeResult, MembershipRequest, RequestMethod, UiEvent,
};
use crate::commands::sender_forms;
use crate::events::{Shared, now_secs};
use crate::info::{
    MetaLookup, RawMember, best_name, group_meta_from, group_meta_lookup, my_membership, own_jids, phone_of_jid,
    server_code,
};
use crate::jid::{classify_error, redact, warn_lib_error};
use crate::media::{avatar_full_path, write_atomic_async};

const MAX_SUBJECT_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 2048;
const MAX_PARTICIPANTS: usize = 256;
const INVALID_LINK: &str = "Not a valid group invite link";
const INVITE_PREFIX: &str = "https://chat.whatsapp.com/";

fn reject(sh: &Shared, message: impl Into<String>) {
    sh.emit(UiEvent::Error { message: message.into() });
}

fn create_failed(sh: &Shared, message: impl Into<String>) {
    sh.emit(UiEvent::GroupCreateFailed { message: message.into() });
}

fn join_failed(sh: &Shared, link: &str, message: impl Into<String>) {
    sh.emit(UiEvent::GroupJoinFailed { link: link.to_string(), message: message.into() });
}

// ------------------------------------------------------------- pure helpers

fn validate_subject(raw: &str) -> Result<String, String> {
    let subject = raw.trim();
    match subject.chars().count() {
        0 => Err("The group needs a name".to_string()),
        n if n > MAX_SUBJECT_CHARS => Err(format!("The group name can have at most {MAX_SUBJECT_CHARS} characters")),
        _ => Ok(subject.to_string()),
    }
}

/// Trims the text; an empty description means "delete it" (`None`).
fn validate_description(raw: Option<String>) -> Result<Option<String>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let text = raw.trim();
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!("The description can have at most {MAX_DESCRIPTION_CHARS} characters"));
    }
    Ok((!text.is_empty()).then(|| text.to_string()))
}

fn validate_group_jid(raw: &str) -> Result<Jid, String> {
    let invalid = || "Invalid group address".to_string();
    if !raw.ends_with("@g.us") {
        return Err(invalid());
    }
    let jid = raw.parse::<Jid>().map_err(|_| invalid())?;
    if jid.is_group() && !jid.user.is_empty() { Ok(jid) } else { Err(invalid()) }
}

/// Parses the people to add/change: phone-number or LID user JIDs, without
/// device suffix, duplicates dropped (first occurrence wins).
fn validate_participants(raw: &[String]) -> Result<Vec<Jid>, String> {
    if raw.is_empty() {
        return Err("Choose at least one person".to_string());
    }
    if raw.len() > MAX_PARTICIPANTS {
        return Err(format!("At most {MAX_PARTICIPANTS} people can be changed at a time"));
    }
    let mut out: Vec<Jid> = Vec::with_capacity(raw.len());
    for entry in raw {
        let jid = entry
            .parse::<Jid>()
            .ok()
            .filter(|j| j.is_pn() || j.is_lid())
            .map(|j| j.to_non_ad())
            .ok_or_else(|| "One of the selected contacts has an invalid address".to_string())?;
        if !out.contains(&jid) {
            out.push(jid);
        }
    }
    Ok(out)
}

/// The invite code of a full link, a scheme-less `chat.whatsapp.com/<code>`
/// or a bare code (alphanumeric, 10-40 characters). Anything else is `None`.
fn parse_invite_code(input: &str) -> Option<String> {
    let input = input.trim();
    let rest = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input);
    let code = match rest.strip_prefix("chat.whatsapp.com/") {
        Some(path) => {
            let path = path.strip_prefix("invite/").unwrap_or(path);
            path.split(['?', '#']).next().unwrap_or("").trim_end_matches('/')
        }
        // A scheme in front of another host is not a WhatsApp link; without
        // a scheme the whole input has to be the code.
        None if rest.len() == input.len() => input,
        None => return None,
    };
    let valid = (10..=40).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then(|| code.to_string())
}

/// Full link for a code or link returned by the library.
fn normalize_invite_link(raw: &str) -> Option<String> {
    parse_invite_code(raw).map(|code| format!("{INVITE_PREFIX}{code}"))
}

/// Plain-English text for a participant's `error` attribute.
fn member_error(code: &str, action: MemberAction) -> (String, bool) {
    let adding = action == MemberAction::Add;
    match code {
        "403" if adding => ("Can't be added because of their privacy settings".to_string(), true),
        "403" | "401" => ("You're not an admin".to_string(), false),
        "409" if adding => ("Already in the group".to_string(), false),
        "409" => ("Not in the group".to_string(), false),
        "404" if adding => ("Not on WhatsApp".to_string(), false),
        "404" => ("Not in the group".to_string(), false),
        other => (format!("Couldn't update (code {other})"), false),
    }
}

/// One participant answer of the server, reduced to what matching needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Answer {
    /// The answer's JID and, when the group is LID-addressed, its phone JID.
    ids: Vec<String>,
    error: Option<String>,
}

fn answers_from(responses: &[ParticipantChangeResponse]) -> Vec<Answer> {
    responses
        .iter()
        .map(|r| Answer {
            ids: std::iter::once(&r.jid)
                .chain(r.phone_number.as_ref())
                .map(|j| j.to_non_ad().to_string())
                .collect(),
            error: r.error.clone(),
        })
        .collect()
}

/// A person the UI asked for: the JID to report results under, plus every
/// other address of the same person (LID / phone form) we know of, so a server
/// answer in either namespace can be matched back to the request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    requested: String,
    aliases: Vec<String>,
}

impl Target {
    fn is_any_of(&self, ids: &[String]) -> bool {
        ids.iter().any(|id| *id == self.requested || self.aliases.contains(id))
    }
}

/// Other addresses of `requested` in the group's member list (a member matches
/// by its own JID or its phone form).
fn member_aliases(requested: &str, members: &[RawMember]) -> Vec<String> {
    members
        .iter()
        .filter(|m| m.jid == requested || m.pn.as_deref() == Some(requested))
        .flat_map(|m| std::iter::once(m.jid.clone()).chain(m.pn.clone()))
        .filter(|id| id != requested)
        .collect()
}

/// The JID to send for `request`. In a LID-addressed group the server expects
/// the member's LID, so a phone request becomes its LID when one is among
/// `aliases`; otherwise (phone group, request already a LID, LID unknown) the
/// request goes out unchanged.
fn wire_jid(request: &Jid, aliases: &[String], lid_group: bool) -> Jid {
    if !lid_group || request.is_lid() {
        return request.clone();
    }
    aliases
        .iter()
        .filter_map(|a| a.parse::<Jid>().ok())
        .find(Jid::is_lid)
        .map_or_else(|| request.clone(), |j| j.to_non_ad())
}

/// One result per requested JID, in request order. A JID the server did not
/// answer for fails with "No answer from WhatsApp".
fn merge_results(requested: &[Target], answers: &[Answer], action: MemberAction) -> Vec<MemberChangeResult> {
    merge_with(requested, answers, |code| member_error(code, action))
}

/// [`merge_results`] with the wording of failures supplied by the caller.
fn merge_with(
    requested: &[Target],
    answers: &[Answer],
    describe: impl Fn(&str) -> (String, bool),
) -> Vec<MemberChangeResult> {
    requested
        .iter()
        .map(|target| (&target.requested, answers.iter().find(|a| target.is_any_of(&a.ids))))
        .map(|(jid, found)| match found {
            None => MemberChangeResult {
                jid: jid.clone(),
                ok: false,
                error: Some("No answer from WhatsApp".to_string()),
                invite_instead: false,
            },
            Some(Answer { error: None, .. }) => MemberChangeResult {
                jid: jid.clone(),
                ok: true,
                error: None,
                invite_instead: false,
            },
            Some(Answer { error: Some(code), .. }) => {
                let (message, invite_instead) = describe(code);
                MemberChangeResult { jid: jid.clone(), ok: false, error: Some(message), invite_instead }
            }
        })
        .collect()
}

/// Plain-English text for the `error` of an approve / reject answer.
fn request_error(code: &str, approve: bool) -> (String, bool) {
    let message = match code {
        "403" | "401" => "You're not an admin".to_string(),
        "404" => "The request is no longer there".to_string(),
        "409" if approve => "Already in the group".to_string(),
        "409" => "The request is no longer there".to_string(),
        other => format!("Couldn't update (code {other})"),
    };
    (message, false)
}

/// The `request_method` attribute of a notification (the listing has none).
fn request_method(m: Option<&str>) -> RequestMethod {
    match m {
        Some("invite_link") => RequestMethod::InviteLink,
        Some("non_admin_add") => RequestMethod::AddedByMember,
        Some(_) => RequestMethod::Other,
        None => RequestMethod::Unknown,
    }
}

/// Requested participants that the metadata returned by group creation does
/// not contain. Heuristic: the create reply lists only the people who were
/// actually added, so anyone missing was refused (in practice by their
/// privacy settings) and is reported as "Couldn't be added" with the offer to
/// send them the invite link instead. `present` holds each participant's JID,
/// LID and phone JID.
fn missing_participants(requested: &[Target], present: &[Vec<String>]) -> Vec<MemberChangeResult> {
    requested
        .iter()
        .filter(|target| !present.iter().any(|ids| target.is_any_of(ids)))
        .map(|target| MemberChangeResult {
            jid: target.requested.clone(),
            ok: false,
            error: Some("Couldn't be added".to_string()),
            invite_instead: true,
        })
        .collect()
}

/// User-facing text of a failed group operation. `what` completes "Could not
/// ...". 401/403 only mean "admins only" for operations that need an admin.
fn describe_failure(code: Option<u16>, class: &str, what: &str, admin_op: bool) -> String {
    match (code, class) {
        (Some(401 | 403), _) if admin_op => "Only admins can change this".to_string(),
        (Some(404), _) => "This group was not found".to_string(),
        (_, "timeout") => "WhatsApp did not answer in time".to_string(),
        (_, "not connected") => "Not connected to WhatsApp".to_string(),
        (_, "rate limited") => "WhatsApp is limiting requests, try again in a moment".to_string(),
        _ => format!("Could not {what}"),
    }
}

fn failure(e: &GroupError, what: &str, admin_op: bool) -> String {
    describe_failure(server_code(e), classify_error(e), what, admin_op)
}

fn invite_info_error(code: Option<u16>) -> String {
    match code {
        Some(400 | 401 | 404 | 406 | 410) => "This invite link is invalid or has been revoked".to_string(),
        Some(416 | 423 | 436) => "This invite link is not available".to_string(),
        _ => "Couldn't load the invite".to_string(),
    }
}

fn join_error(code: Option<u16>) -> Option<&'static str> {
    Some(match code? {
        304 => "You're already in this group",
        400 | 404 | 410 => "This invite link is invalid or has been revoked",
        403 | 405 => "You can't join this group with this link",
        412 | 419 => "This group is full",
        423 => "This group is locked",
        _ => return None,
    })
}

// ------------------------------------------------------------ shared helpers

fn group_jid_or_reject(sh: &Shared, raw: &str) -> Option<Jid> {
    match validate_group_jid(raw) {
        Ok(jid) => Some(jid),
        Err(message) => {
            reject(sh, message);
            None
        }
    }
}

/// Forgets what the info panel knows about the group and re-emits it.
async fn refresh(sh: &Shared, client: &Arc<Client>, jid: &str) {
    sh.info.invalidate(jid);
    crate::info::fetch(sh.clone(), client.clone(), jid.to_string()).await;
}

/// Metadata through the paced metadata slot.
async fn fetch_metadata(sh: &Shared, client: &Client, jid: &Jid) -> Result<GroupMetadata, GroupError> {
    let Some(_slot) = sh.groups.metadata_slot().await else {
        return Err(GroupError::InvalidRequest("requests are paused".to_string()));
    };
    client.groups().get_metadata(jid).await
}

/// Matching info for each requested person: the addresses the group's member
/// list knows plus both forms from the library's LID<->PN mapping.
async fn targets_for(client: &Client, people: &[Jid], members: &[RawMember]) -> Vec<Target> {
    let mut out = Vec::with_capacity(people.len());
    for person in people {
        let requested = person.to_string();
        let mut aliases = member_aliases(&requested, members);
        let (pn, lid) = sender_forms(client, person).await;
        for form in [pn, lid].into_iter().flatten().map(|j| j.to_string()) {
            if form != requested && !aliases.contains(&form) {
                aliases.push(form);
            }
        }
        out.push(Target { requested, aliases });
    }
    out
}

/// Makes sure the group has a chat row, naming it when a subject is known.
fn ensure_chat_row(sh: &Shared, jid: &str, subject: Option<&str>) {
    match sh.store.upsert_chat(jid, ChatKind::Group, subject.filter(|s| !s.is_empty())) {
        Ok(()) => {
            if let Some(subject) = subject {
                crate::groups::store_subject(sh, jid, subject);
            }
            sh.emit(UiEvent::ChatsChanged);
        }
        Err(e) => log::error!("creating the chat row of {} failed: {e}", redact(jid)),
    }
}

/// Sets (`Some`) or removes (`None`) the group's picture. `Err` carries the
/// message for the user.
async fn apply_picture(
    sh: &Shared,
    client: &Arc<Client>,
    jid: &Jid,
    jid_str: &str,
    path: Option<PathBuf>,
) -> Result<(), String> {
    let context = format!("group picture change for {}", redact(jid_str));
    let Some(path) = path else {
        client.groups().remove_profile_picture(jid.clone()).await.map_err(|e| {
            warn_lib_error(&context, &e);
            failure(&e, "remove the group picture", true)
        })?;
        if let Err(e) = sh.store.clear_contact_avatar(jid_str) {
            log::error!("clearing the group avatar failed: {e}");
        }
        crate::avatar::invalidate_and_refetch(sh, client, jid_str.to_string());
        return Ok(());
    };
    let encoded = tokio::task::spawn_blocking(move || {
        read_picture_file(&path).and_then(|data| reencode_profile_picture(&data))
    })
    .await;
    let jpeg = match encoded {
        Ok(Ok(jpeg)) => jpeg,
        Ok(Err(message)) => return Err(message),
        Err(e) => {
            log::error!("picture encoding task failed: {e}");
            return Err("The picture could not be processed".to_string());
        }
    };
    let id = client
        .groups()
        .set_profile_picture(jid.clone(), jpeg.clone())
        .await
        .map(|response| response.id)
        .map_err(|e| {
            warn_lib_error(&context, &e);
            failure(&e, "change the group picture", true)
        })?;
    let file = avatar_full_path(&sh.media.dir, jid_str, &id);
    match write_atomic_async(file.clone(), jpeg).await {
        Ok(()) => {
            if let Err(e) = sh.store.set_contact_avatar(jid_str, &id, &file.to_string_lossy()) {
                log::error!("storing the group avatar failed: {e}");
            }
            if let Err(e) = sh.store.mark_avatar_checked(jid_str, now_secs()) {
                log::error!("mark_avatar_checked failed: {e}");
            }
            sh.emit(UiEvent::AvatarReady { jid: jid_str.to_string(), path: file });
        }
        Err(e) => log::error!("caching the group picture failed: {e:#}"),
    }
    Ok(())
}

// ------------------------------------------------------------------ commands

pub async fn create_group(sh: &Shared, client: &Arc<Client>, subject: &str, participants: &[String], picture: Option<PathBuf>) {
    let subject = match validate_subject(subject) {
        Ok(s) => s,
        Err(message) => return create_failed(sh, message),
    };
    let jids = match validate_participants(participants) {
        Ok(j) => j,
        Err(message) => return create_failed(sh, message),
    };
    let options = jids.iter().fold(GroupCreateOptions::new(subject.clone()), |options, jid| {
        options.with_participant(GroupParticipantOptions::new(jid.clone()))
    });
    let meta = match client.groups().create_group(options).await {
        Ok(created) => created.metadata,
        Err(e) => {
            warn_lib_error("CreateGroup failed", &e);
            return create_failed(sh, failure(&e, "create the group", false));
        }
    };
    let group_jid = meta.id.to_non_ad().to_string();
    log::info!("CreateGroup: created {}", redact(&group_jid));
    let name = if meta.subject.is_empty() { &subject } else { &meta.subject };
    ensure_chat_row(sh, &group_jid, Some(name));

    if let Some(path) = picture {
        if let Err(message) = apply_picture(sh, client, &meta.id, &group_jid, Some(path)).await {
            reject(sh, format!("The group was created, but its picture could not be set: {message}"));
        }
    }

    let requested = targets_for(client, &jids, &[]).await;
    let present: Vec<Vec<String>> = meta
        .participants
        .iter()
        .map(|p| {
            std::iter::once(&p.jid)
                .chain(p.lid.as_ref())
                .chain(p.phone_number.as_ref())
                .map(|j| j.to_non_ad().to_string())
                .collect()
        })
        .collect();
    sh.emit(UiEvent::GroupCreated {
        group_jid,
        failed: missing_participants(&requested, &present),
    });
}

pub async fn update_members(sh: &Shared, client: &Arc<Client>, group_jid: &str, jids: &[String], action: MemberAction) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    let people = match validate_participants(jids) {
        Ok(p) => p,
        Err(message) => return reject(sh, message),
    };
    // Changing existing members needs fresh metadata: in a LID-addressed group
    // the server wants (and answers with) the members' LIDs. Adding sends the
    // phone form the UI has, since the person is not in the member list yet.
    let (members, lid_group) = if action == MemberAction::Add {
        (Vec::new(), false)
    } else {
        members_and_mode(sh, client, &group, group_jid).await
    };
    let targets = targets_for(client, &people, &members).await;
    let wire: Vec<Jid> = people
        .iter()
        .zip(&targets)
        .map(|(person, target)| {
            let sent = wire_jid(person, &target.aliases, lid_group);
            if lid_group && !sent.is_lid() {
                log::debug!("no LID known for {} in a LID group, sending the phone form", redact(&person.to_string()));
            }
            sent
        })
        .collect();
    let groups = client.groups();
    let response = match action {
        MemberAction::Add => groups.add_participants(group.clone(), &wire).await,
        MemberAction::Remove => groups.remove_participants(group.clone(), &wire).await,
        MemberAction::Promote => groups.promote_participants(group.clone(), &wire).await,
        MemberAction::Demote => groups.demote_participants(group.clone(), &wire).await,
    };
    let responses = match response {
        Ok(r) => r,
        Err(e) => {
            warn_lib_error(&format!("UpdateGroupMembers failed for {}", redact(group_jid)), &e);
            return reject(sh, failure(&e, "update the members", true));
        }
    };
    let results = merge_results(&targets, &answers_from(&responses), action);
    log::info!(
        "UpdateGroupMembers: {} of {} ok",
        results.iter().filter(|r| r.ok).count(),
        results.len()
    );
    let group_str = group.to_string();
    sh.emit(UiEvent::GroupMembersResult { group_jid: group_str.clone(), action, results });
    refresh(sh, client, &group_str).await;
}

/// The member list and whether the group is LID-addressed; unknown (empty,
/// false) when the metadata cannot be loaded.
async fn members_and_mode(sh: &Shared, client: &Client, group: &Jid, group_jid: &str) -> (Vec<RawMember>, bool) {
    match fetch_metadata(sh, client, group).await {
        Ok(raw) => {
            let meta = group_meta_from(&raw);
            (meta.members, meta.addressing == AddressingMode::Lid)
        }
        Err(e) => {
            log::debug!("member list of {} unavailable, sending addresses as given: {e}", redact(group_jid));
            (Vec::new(), false)
        }
    }
}

/// How many people wait for approval; 0 when the list cannot be loaded.
pub(crate) async fn pending_count(client: &Client, group: &Jid) -> u32 {
    match client.groups().get_membership_requests(group.clone()).await {
        Ok(list) => u32::try_from(list.len()).unwrap_or(u32::MAX),
        Err(e) => {
            log::debug!("pending requests of {} unavailable: {e}", redact(&group.to_string()));
            0
        }
    }
}

/// The listing entry shown to the user: the address to answer with, a name
/// (address book, push name, else the phone) and the phone when known.
async fn describe_request(sh: &Shared, client: &Client, r: &LibRequest) -> MembershipRequest {
    let jid = r.jid.to_non_ad();
    let (pn, lid) = sender_forms(client, &jid).await;
    let phone = pn.as_ref().and_then(|j| phone_of_jid(&j.to_string()));
    let name = [pn, lid, Some(jid.clone())]
        .into_iter()
        .flatten()
        .find_map(|j| sh.store.contact(&j.to_string()).ok().flatten().as_ref().and_then(best_name))
        .or_else(|| phone.as_ref().map(|p| format!("+{p}")));
    MembershipRequest {
        jid: jid.to_string(),
        name,
        phone,
        requested_at: r.request_time.and_then(|t| i64::try_from(t).ok()),
        method: request_method(None),
    }
}

/// Oldest request first, so the list reads in the order people asked.
fn sort_requests(list: &mut [MembershipRequest]) {
    list.sort_by_key(|r| r.requested_at.unwrap_or(i64::MAX));
}

async fn load_requests(sh: &Shared, client: &Client, group: &Jid) -> Result<Vec<MembershipRequest>, GroupError> {
    let raw = client.groups().get_membership_requests(group.clone()).await?;
    let mut out = Vec::with_capacity(raw.len());
    for r in &raw {
        out.push(describe_request(sh, client, r).await);
    }
    sort_requests(&mut out);
    Ok(out)
}

pub async fn fetch_requests(sh: &Shared, client: &Arc<Client>, group_jid: &str) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    let group_str = group.to_string();
    match load_requests(sh, client, &group).await {
        Ok(requests) => {
            log::info!("FetchMembershipRequests: {} pending", requests.len());
            sh.emit(UiEvent::MembershipRequests { group_jid: group_str, requests: Some(requests) });
        }
        Err(e) => {
            warn_lib_error(&format!("FetchMembershipRequests failed for {}", redact(group_jid)), &e);
            sh.emit(UiEvent::MembershipRequests { group_jid: group_str, requests: None });
            reject(sh, failure(&e, "load the join requests", true));
        }
    }
}

pub async fn answer_requests(sh: &Shared, client: &Arc<Client>, group_jid: &str, jids: &[String], approve: bool) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    let people = match validate_participants(jids) {
        Ok(p) => p,
        Err(message) => return reject(sh, message),
    };
    // Like member changes: a LID-addressed group wants the LIDs.
    let (members, lid_group) = members_and_mode(sh, client, &group, group_jid).await;
    let targets = targets_for(client, &people, &members).await;
    let wire: Vec<Jid> = people.iter().zip(&targets).map(|(p, t)| wire_jid(p, &t.aliases, lid_group)).collect();
    let groups = client.groups();
    let response = if approve {
        groups.approve_membership_requests(group.clone(), &wire).await
    } else {
        groups.reject_membership_requests(group.clone(), &wire).await
    };
    let responses = match response {
        Ok(r) => r,
        Err(e) => {
            warn_lib_error(&format!("AnswerMembershipRequests failed for {}", redact(group_jid)), &e);
            return reject(sh, failure(&e, "answer the join requests", true));
        }
    };
    let results = merge_with(&targets, &answers_from(&responses), |code| request_error(code, approve));
    log::info!(
        "AnswerMembershipRequests ({}): {} of {} ok",
        if approve { "approve" } else { "reject" },
        results.iter().filter(|r| r.ok).count(),
        results.len()
    );
    let group_str = group.to_string();
    sh.emit(UiEvent::MembershipRequestsAnswered { group_jid: group_str.clone(), approve, results });
    fetch_requests(sh, client, &group_str).await;
    refresh(sh, client, &group_str).await;
}

pub async fn set_subject(sh: &Shared, client: &Arc<Client>, group_jid: &str, subject: &str) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    let subject = match validate_subject(subject) {
        Ok(s) => s,
        Err(message) => return reject(sh, message),
    };
    let validated = match GroupSubject::new(subject.clone()) {
        Ok(v) => v,
        Err(_) => return reject(sh, format!("The group name can have at most {MAX_SUBJECT_CHARS} characters")),
    };
    if let Err(e) = client.groups().set_subject(group.clone(), validated).await {
        warn_lib_error(&format!("SetGroupSubject failed for {}", redact(group_jid)), &e);
        return reject(sh, failure(&e, "change the group name", true));
    }
    log::info!("SetGroupSubject: ok");
    let group_str = group.to_string();
    ensure_chat_row(sh, &group_str, Some(&subject));
    refresh(sh, client, &group_str).await;
}

pub async fn set_description(sh: &Shared, client: &Arc<Client>, group_jid: &str, description: Option<String>) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    let text = match validate_description(description) {
        Ok(t) => t,
        Err(message) => return reject(sh, message),
    };
    let what = "change the description";
    // The server only accepts the change against the description id we saw;
    // when someone else changed it meanwhile, read it again and retry once.
    let mut conflicts = 0;
    let outcome = loop {
        let meta = match fetch_metadata(sh, client, &group).await {
            Ok(m) => m,
            Err(e) => break Err(e),
        };
        let new = match text.as_ref().map(|t| GroupDescription::new(t.clone())).transpose() {
            Ok(n) => n,
            Err(_) => return reject(sh, format!("The description can have at most {MAX_DESCRIPTION_CHARS} characters")),
        };
        let prev = match meta.description_id.as_deref() {
            Some(id) => PreviousDescription::Id(id),
            None => PreviousDescription::Absent,
        };
        match client.groups().set_description(group.clone(), new, prev).await {
            Err(GroupError::DescriptionConflict) if conflicts == 0 => conflicts += 1,
            other => break other,
        }
    };
    match outcome {
        Ok(()) => {
            log::info!("SetGroupDescription: ok");
            refresh(sh, client, &group.to_string()).await;
        }
        Err(GroupError::DescriptionConflict) => {
            reject(sh, "The description was changed by someone else, please try again");
        }
        Err(e) => {
            warn_lib_error(&format!("SetGroupDescription failed for {}", redact(group_jid)), &e);
            reject(sh, failure(&e, what, true));
        }
    }
}

pub async fn set_picture(sh: &Shared, client: &Arc<Client>, group_jid: &str, path: Option<PathBuf>) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    let group_str = group.to_string();
    match apply_picture(sh, client, &group, &group_str, path).await {
        Ok(()) => {
            log::info!("SetGroupPicture: ok");
            refresh(sh, client, &group_str).await;
        }
        Err(message) => reject(sh, message),
    }
}

pub async fn set_setting(sh: &Shared, client: &Arc<Client>, group_jid: &str, setting: GroupSetting) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    let groups = client.groups();
    let result = match setting {
        GroupSetting::AnnounceOnly(on) => groups.set_announce(group.clone(), on).await,
        GroupSetting::Locked(on) => groups.set_locked(group.clone(), on).await,
        GroupSetting::ApprovalRequired(on) => {
            let mode = if on { MembershipApprovalMode::On } else { MembershipApprovalMode::Off };
            groups.set_membership_approval(group.clone(), mode).await
        }
    };
    match result {
        Ok(()) => {
            log::info!("SetGroupSetting: ok");
            refresh(sh, client, &group.to_string()).await;
        }
        Err(e) => {
            warn_lib_error(&format!("SetGroupSetting failed for {}", redact(group_jid)), &e);
            reject(sh, failure(&e, "change the group setting", true));
        }
    }
}

pub async fn leave(sh: &Shared, client: &Arc<Client>, group_jid: &str) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    if let Err(e) = client.groups().leave(group.clone()).await {
        warn_lib_error(&format!("LeaveGroup failed for {}", redact(group_jid)), &e);
        return reject(sh, failure(&e, "leave the group", false));
    }
    log::info!("LeaveGroup: ok");
    // The metadata query now fails with "not a participant", which info.rs
    // turns into i_am_member = false.
    refresh(sh, client, &group.to_string()).await;
}

pub async fn invite_link(sh: &Shared, client: &Arc<Client>, group_jid: &str, reset: bool) {
    let Some(group) = group_jid_or_reject(sh, group_jid) else {
        return;
    };
    match client.groups().get_invite_link(group.clone(), reset).await {
        Ok(raw) => match normalize_invite_link(&raw) {
            Some(link) => sh.emit(UiEvent::InviteLink { group_jid: group.to_string(), link }),
            None => {
                log::warn!("the invite link answer has an unexpected shape");
                reject(sh, "Could not get the invite link");
            }
        },
        Err(e) => {
            warn_lib_error(&format!("FetchInviteLink failed for {}", redact(group_jid)), &e);
            reject(sh, failure(&e, "get the invite link", true));
        }
    }
}

pub async fn preview_invite(sh: &Shared, client: &Arc<Client>, link: &str) {
    let answer = |error: String| {
        sh.emit(UiEvent::InvitePreview(InvitePreview {
            link: link.to_string(),
            error: Some(error),
            ..InvitePreview::default()
        }));
    };
    let Some(code) = parse_invite_code(link) else {
        return answer(INVALID_LINK.to_string());
    };
    let meta = {
        let Some(_slot) = sh.groups.metadata_slot().await else {
            return answer("Couldn't load the invite".to_string());
        };
        match client.groups().get_invite_info(&code).await {
            Ok(meta) => meta,
            Err(e) => {
                warn_lib_error("PreviewInvite failed", &e);
                return answer(invite_info_error(server_code(&e)));
            }
        }
    };
    let group_jid = meta.id.to_non_ad().to_string();
    let me = own_jids(client);
    let raw: Vec<_> = meta
        .participants
        .iter()
        .map(|p| crate::info::RawMember {
            jid: p.jid.to_non_ad().to_string(),
            pn: p.phone_number.as_ref().map(|j| j.to_non_ad().to_string()),
            is_admin: false,
            is_super_admin: false,
        })
        .collect();
    let mut already_member = my_membership(&raw, &me).1;
    if !already_member && matches!(sh.store.chat(&group_jid), Ok(Some(_))) {
        if let Ok(jid) = group_jid.parse::<Jid>() {
            already_member = match group_meta_lookup(sh, client, &jid, &group_jid).await {
                MetaLookup::Found(m) => my_membership(&m.members, &me).1,
                MetaLookup::NotMember | MetaLookup::Unavailable => false,
            };
        }
    }
    let count = meta.size.map_or(meta.participants.len(), |n| n as usize);
    sh.emit(UiEvent::InvitePreview(InvitePreview {
        link: link.to_string(),
        subject: Some(meta.subject.clone()),
        description: meta.description.clone().filter(|d| !d.is_empty()),
        member_count: u32::try_from(count).unwrap_or(u32::MAX),
        group_jid: already_member.then_some(group_jid),
        already_member,
        approval_required: meta.membership_approval,
        error: None,
    }));
}

pub async fn join(sh: &Shared, client: &Arc<Client>, link: &str) {
    let Some(code) = parse_invite_code(link) else {
        return join_failed(sh, link, INVALID_LINK);
    };
    match client.groups().join_with_invite_code(&code).await {
        Ok(JoinGroupResult::Joined(jid)) => {
            let group_jid = jid.to_non_ad().to_string();
            log::info!("JoinGroup: joined {}", redact(&group_jid));
            ensure_chat_row(sh, &group_jid, None);
            crate::groups::ensure_name(sh, client, &group_jid);
            sh.info.invalidate(&group_jid);
            sh.emit(UiEvent::GroupJoined { group_jid, pending: false });
        }
        Ok(JoinGroupResult::PendingApproval(jid)) => {
            log::info!("JoinGroup: waiting for approval of {}", redact(&jid.to_string()));
            sh.emit(UiEvent::GroupJoined { group_jid: jid.to_non_ad().to_string(), pending: true });
        }
        Err(e) => {
            warn_lib_error("JoinGroup failed", &e);
            let message = join_error(server_code(&e))
                .map_or_else(|| failure(&e, "join the group", false), str::to_string);
            join_failed(sh, link, message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PN: &str = "4917011@s.whatsapp.net";
    const CODE: &str = "AbCdEfGhIjKlMnOpQrSt12";

    #[test]
    fn invite_code_from_link_variants() {
        let full = format!("https://chat.whatsapp.com/{CODE}");
        assert_eq!(parse_invite_code(&full).as_deref(), Some(CODE));
        assert_eq!(parse_invite_code(&format!("  {full}\n")).as_deref(), Some(CODE));
        assert_eq!(parse_invite_code(&format!("http://chat.whatsapp.com/{CODE}")).as_deref(), Some(CODE));
        assert_eq!(parse_invite_code(&format!("chat.whatsapp.com/{CODE}")).as_deref(), Some(CODE));
        assert_eq!(parse_invite_code(&format!("{full}?mode=r_c")).as_deref(), Some(CODE));
        assert_eq!(parse_invite_code(&format!("{full}/")).as_deref(), Some(CODE));
        assert_eq!(parse_invite_code(&format!("https://chat.whatsapp.com/invite/{CODE}")).as_deref(), Some(CODE));
        assert_eq!(parse_invite_code(CODE).as_deref(), Some(CODE));
    }

    #[test]
    fn invalid_invite_links_are_rejected() {
        for bad in [
            "",
            "   ",
            "short",
            "https://chat.whatsapp.com/",
            "https://chat.whatsapp.com/short",
            "https://example.com/AbCdEfGhIjKlMnOpQrSt12",
            "ftp://chat.whatsapp.com/AbCdEfGhIjKlMnOpQrSt12",
            "AbCdEfGhIjKl/MnOpQrSt12",
            "AbCdEfGhIjKl MnOpQrSt12",
            "AbCdEfGhIjKl-MnOpQrSt12",
            &"a".repeat(41),
        ] {
            assert_eq!(parse_invite_code(bad), None, "{bad:?}");
        }
        assert!(parse_invite_code(&"a".repeat(40)).is_some());
        assert!(parse_invite_code(&"a".repeat(10)).is_some());
    }

    #[test]
    fn invite_link_is_normalised() {
        let want = format!("https://chat.whatsapp.com/{CODE}");
        assert_eq!(normalize_invite_link(CODE).as_deref(), Some(want.as_str()));
        assert_eq!(normalize_invite_link(&want).as_deref(), Some(want.as_str()));
        assert_eq!(normalize_invite_link("nonsense!"), None);
    }

    #[test]
    fn request_answers_use_request_wording() {
        assert_eq!(request_error("403", true), ("You're not an admin".to_string(), false));
        assert_eq!(request_error("404", false), ("The request is no longer there".to_string(), false));
        assert_eq!(request_error("409", true), ("Already in the group".to_string(), false));
        assert_eq!(request_error("500", false), ("Couldn't update (code 500)".to_string(), false));
    }

    #[test]
    fn request_results_match_phone_answers_to_lid_requests() {
        let target = Target { requested: "200@lid".into(), aliases: vec!["491701234567@s.whatsapp.net".into()] };
        let answer = |id: &str, error: Option<&str>| Answer { ids: vec![id.into()], error: error.map(String::from) };
        let ok = merge_with(&[target.clone()], &[answer("491701234567@s.whatsapp.net", None)], |c| request_error(c, true));
        assert!(ok[0].ok);
        assert_eq!(ok[0].jid, "200@lid", "reported under the requested address");
        let refused = merge_with(&[target.clone()], &[answer("200@lid", Some("404"))], |c| request_error(c, false));
        assert_eq!(refused[0].error.as_deref(), Some("The request is no longer there"));
        let silent = merge_with(&[target], &[], |c| request_error(c, true));
        assert_eq!(silent[0].error.as_deref(), Some("No answer from WhatsApp"));
    }

    #[test]
    fn request_methods_and_order() {
        assert_eq!(request_method(Some("invite_link")), RequestMethod::InviteLink);
        assert_eq!(request_method(Some("non_admin_add")), RequestMethod::AddedByMember);
        assert_eq!(request_method(Some("linked_group_join")), RequestMethod::Other);
        assert_eq!(request_method(None), RequestMethod::Unknown);
        let at = |jid: &str, t: Option<i64>| MembershipRequest { jid: jid.into(), requested_at: t, ..Default::default() };
        let mut list = vec![at("c", None), at("b", Some(20)), at("a", Some(10))];
        sort_requests(&mut list);
        assert_eq!(list.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn subject_is_trimmed_and_bounded() {
        assert_eq!(validate_subject("  Family  ").as_deref(), Ok("Family"));
        assert!(validate_subject("   ").is_err());
        assert!(validate_subject("").is_err());
        assert!(validate_subject(&"x".repeat(100)).is_ok());
        assert!(validate_subject(&"x".repeat(101)).is_err());
        assert!(validate_subject(&"\u{e9}".repeat(100)).is_ok(), "counts characters, not bytes");
    }

    #[test]
    fn description_rules() {
        assert_eq!(validate_description(None), Ok(None));
        assert_eq!(validate_description(Some("  ".into())), Ok(None), "blank deletes");
        assert_eq!(validate_description(Some(" hi ".into())), Ok(Some("hi".to_string())));
        assert!(validate_description(Some("x".repeat(2048))).is_ok());
        assert!(validate_description(Some("x".repeat(2049))).is_err());
    }

    #[test]
    fn group_jids_must_be_groups() {
        assert!(validate_group_jid("120363000000000001@g.us").is_ok());
        assert!(validate_group_jid("1-2@g.us").is_ok());
        assert!(validate_group_jid(PN).is_err());
        assert!(validate_group_jid("").is_err());
        assert!(validate_group_jid("@g.us").is_err());
    }

    #[test]
    fn participants_are_parsed_deduplicated_and_bounded() {
        assert!(validate_participants(&[]).is_err());
        let ok = validate_participants(&[PN.into(), "4917011:3@s.whatsapp.net".into(), "100@lid".into()]).unwrap_or_default();
        let ok: Vec<String> = ok.iter().map(ToString::to_string).collect();
        assert_eq!(ok, vec![PN.to_string(), "100@lid".to_string()]);
        assert!(validate_participants(&["1-2@g.us".into()]).is_err(), "a group is not a person");
        assert!(validate_participants(&["garbage".into()]).is_err());
        assert!(validate_participants(&[PN.into(), "garbage".into()]).is_err());
        let many: Vec<String> = (0..257).map(|i| format!("49170{i}@s.whatsapp.net")).collect();
        assert!(validate_participants(&many).is_err());
        assert!(validate_participants(&many[..256]).is_ok());
    }

    #[test]
    fn member_error_messages() {
        assert_eq!(
            member_error("403", MemberAction::Add),
            ("Can't be added because of their privacy settings".to_string(), true)
        );
        assert_eq!(member_error("403", MemberAction::Remove), ("You're not an admin".to_string(), false));
        assert_eq!(member_error("401", MemberAction::Add), ("You're not an admin".to_string(), false));
        assert_eq!(member_error("409", MemberAction::Add), ("Already in the group".to_string(), false));
        assert_eq!(member_error("409", MemberAction::Remove), ("Not in the group".to_string(), false));
        assert_eq!(member_error("404", MemberAction::Promote), ("Not in the group".to_string(), false));
        assert_eq!(member_error("500", MemberAction::Demote), ("Couldn't update (code 500)".to_string(), false));
    }

    fn answer(ids: &[&str], error: Option<&str>) -> Answer {
        Answer { ids: ids.iter().map(ToString::to_string).collect(), error: error.map(Into::into) }
    }

    #[test]
    fn results_follow_the_request_order_and_flag_missing_answers() {
        let requested = targets(&[PN, "4917022@s.whatsapp.net", "4917033@s.whatsapp.net", "4917044@s.whatsapp.net"]);
        let answers = [
            answer(&["4917022@s.whatsapp.net"], Some("403")),
            // LID-addressed group: the answer's own jid is the LID, the phone jid matches.
            answer(&["200@lid", PN], None),
            answer(&["4917033@s.whatsapp.net"], Some("409")),
        ];
        let results = merge_results(&requested, &answers, MemberAction::Add);
        assert_eq!(results.len(), 4);
        assert_eq!(results[0], MemberChangeResult { jid: PN.into(), ok: true, error: None, invite_instead: false });
        assert!(!results[1].ok && results[1].invite_instead);
        assert_eq!(results[1].error.as_deref(), Some("Can't be added because of their privacy settings"));
        assert_eq!(results[2].error.as_deref(), Some("Already in the group"));
        assert!(!results[2].invite_instead);
        assert_eq!(results[3].error.as_deref(), Some("No answer from WhatsApp"));
        assert!(!results[3].ok);
    }

    #[test]
    fn creation_reports_people_missing_from_the_new_group() {
        let requested = targets(&[PN, "4917022@s.whatsapp.net", "4917033@s.whatsapp.net"]);
        let present = vec![
            vec!["999@lid".to_string(), PN.to_string()],
            vec!["4917033@s.whatsapp.net".to_string()],
            vec!["1@lid".to_string()],
        ];
        let failed = missing_participants(&requested, &present);
        assert_eq!(
            failed,
            vec![MemberChangeResult {
                jid: "4917022@s.whatsapp.net".into(),
                ok: false,
                error: Some("Couldn't be added".into()),
                invite_instead: true,
            }]
        );
        let everyone: Vec<String> = requested.iter().map(|t| t.requested.clone()).collect();
        assert!(missing_participants(&requested, &[everyone]).is_empty());
    }

    fn targets(jids: &[&str]) -> Vec<Target> {
        jids.iter().map(|j| Target { requested: j.to_string(), aliases: vec![] }).collect()
    }

    fn member(jid: &str, pn: Option<&str>) -> RawMember {
        RawMember { jid: jid.into(), pn: pn.map(Into::into), is_admin: false, is_super_admin: false }
    }

    #[test]
    fn phone_request_matches_a_lid_only_answer_through_aliases() {
        let members = [member("200@lid", Some(PN)), member("300@lid", None)];
        let target = Target { requested: PN.to_string(), aliases: member_aliases(PN, &members) };
        assert_eq!(target.aliases, vec!["200@lid".to_string()]);

        let results = merge_results(&[target.clone()], &[answer(&["200@lid"], None)], MemberAction::Remove);
        assert_eq!(results, vec![MemberChangeResult { jid: PN.into(), ok: true, error: None, invite_instead: false }]);

        let unrelated = merge_results(&[target.clone()], &[answer(&["300@lid"], None)], MemberAction::Remove);
        assert_eq!(unrelated[0].error.as_deref(), Some("No answer from WhatsApp"));

        // Without aliases the same answer would not be found.
        let bare = merge_results(&targets(&[PN]), &[answer(&["200@lid"], None)], MemberAction::Remove);
        assert!(!bare[0].ok);
    }

    #[test]
    fn creation_matches_requested_phone_against_lid_participants() {
        let target = Target { requested: PN.to_string(), aliases: vec!["200@lid".to_string()] };
        let other = Target { requested: "4917022@s.whatsapp.net".to_string(), aliases: vec![] };
        let failed = missing_participants(&[target, other], &[vec!["200@lid".to_string()], vec!["1@lid".to_string()]]);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].jid, "4917022@s.whatsapp.net");
    }

    #[test]
    fn member_aliases_ignore_unrelated_members() {
        let members = [member("200@lid", Some(PN)), member("300@lid", Some("4917099@s.whatsapp.net"))];
        assert!(member_aliases("4917055@s.whatsapp.net", &members).is_empty());
        assert_eq!(member_aliases("200@lid", &members), vec![PN.to_string()]);
    }

    #[test]
    fn wire_jid_uses_the_lid_only_in_lid_groups() {
        let pn: Jid = PN.parse().expect("jid");
        let aliases = vec!["200:3@lid".to_string()];
        let lid: Jid = "200@lid".parse().expect("jid");
        assert_eq!(wire_jid(&pn, &aliases, true), lid);
        assert_eq!(wire_jid(&pn, &aliases, false), pn, "phone group keeps the phone form");
        assert_eq!(wire_jid(&pn, &[], true), pn, "no LID known: phone form");
        assert_eq!(wire_jid(&lid, &[PN.to_string()], true), lid);
    }

    #[test]
    fn failures_are_described_in_plain_english() {
        assert_eq!(describe_failure(Some(403), "not authorized", "x", true), "Only admins can change this");
        assert_eq!(describe_failure(Some(401), "not authorized", "x", true), "Only admins can change this");
        assert_eq!(describe_failure(Some(403), "not authorized", "create the group", false), "Could not create the group");
        assert_eq!(describe_failure(Some(404), "not found", "x", true), "This group was not found");
        assert_eq!(describe_failure(None, "timeout", "x", true), "WhatsApp did not answer in time");
        assert_eq!(describe_failure(None, "not connected", "x", true), "Not connected to WhatsApp");
        assert_eq!(describe_failure(None, "other", "leave the group", true), "Could not leave the group");
    }

    #[test]
    fn invite_and_join_errors() {
        assert_eq!(invite_info_error(Some(410)), "This invite link is invalid or has been revoked");
        assert_eq!(invite_info_error(None), "Couldn't load the invite");
        assert_eq!(join_error(Some(419)), Some("This group is full"));
        assert_eq!(join_error(Some(304)), Some("You're already in this group"));
        assert_eq!(join_error(Some(500)), None);
        assert_eq!(join_error(None), None);
    }
}
