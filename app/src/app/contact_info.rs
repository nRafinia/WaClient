//! Contact / group info panel: pure conversion from the engine's
//! `ContactInfo` into display-ready strings, plus phone formatting and member
//! sorting / filtering. No app-state coupling, so the wiring only has to copy
//! a `ContactInfoView` into the Slint `ContactInfoModel`.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use chrono::{DateTime, Datelike, Local, TimeZone};
use wa_engine::{BusinessProfile, ContactInfo, GroupInfo, GroupMember};
use wa_store::MessageRow;

use crate::links;
use crate::models::initials;

/// Display-ready panel content. `tint_key` feeds `models::tint_for`.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct ContactInfoView {
    pub jid: String,
    pub title: String,
    pub name: String,
    pub phone: String,
    pub subtitle: String,
    pub about: String,
    pub about_date: String,
    pub is_group: bool,
    pub is_business: bool,
    pub loading: bool,
    pub picture_path: Option<PathBuf>,
    pub initials: String,
    pub tint_key: String,
    pub description: String,
    pub description_long: bool,
    pub created_label: String,
    pub members: Vec<MemberView>,
    pub business: BusinessView,
    pub controls: GroupControls,
}

/// What the viewer may do in the open group, and the group's admin settings.
/// Nothing is offered until the engine's network-backed answer has arrived,
/// because `i_am_admin` / `i_am_member` are only trustworthy then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct GroupControls {
    /// Add members, invite link, settings, member menus.
    pub is_admin: bool,
    /// Rename, description, photo: admins always, members unless the group is locked.
    pub can_edit_info: bool,
    pub can_leave: bool,
    /// We are no longer a participant.
    pub left: bool,
    pub announce_only: bool,
    pub locked: bool,
    pub approval_required: bool,
    /// People waiting for approval (admins only).
    pub pending_requests: u32,
}

pub(super) fn group_controls(g: &GroupInfo, complete: bool) -> GroupControls {
    let settings = GroupControls {
        announce_only: g.announce_only,
        locked: g.locked,
        approval_required: g.approval_required,
        ..GroupControls::default()
    };
    if !complete {
        return settings;
    }
    if !g.i_am_member {
        return GroupControls { left: true, ..settings };
    }
    GroupControls {
        is_admin: g.i_am_admin,
        can_edit_info: g.i_am_admin || !g.locked,
        can_leave: true,
        pending_requests: if g.i_am_admin && g.approval_required { g.pending_requests } else { 0 },
        ..settings
    }
}

/// Which entries the menu of a member row offers.
pub(super) fn member_menu(viewer_is_admin: bool, m: &MemberView) -> (bool, bool, bool) {
    // The creator cannot be demoted or removed, and we never act on ourselves here.
    let actionable = viewer_is_admin && !m.is_me && !m.is_super_admin;
    (actionable && !m.is_admin, actionable && m.is_admin, actionable)
}

/// WhatsApp Business details with empty fields dropped.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct BusinessView {
    pub category: String,
    pub description: String,
    pub address: String,
    pub email: String,
    pub websites: Vec<String>,
    pub hours: Vec<String>,
}

impl BusinessView {
    pub fn is_empty(&self) -> bool {
        self.category.is_empty()
            && self.description.is_empty()
            && self.address.is_empty()
            && self.email.is_empty()
            && self.websites.is_empty()
            && self.hours.is_empty()
    }
}

fn business_view(b: &BusinessProfile) -> BusinessView {
    let one = |s: &Option<String>| non_empty(s).unwrap_or_default().to_string();
    let many = |v: &[String]| -> Vec<String> {
        v.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).collect()
    };
    BusinessView {
        category: one(&b.category),
        description: one(&b.description),
        address: one(&b.address),
        email: Some(one(&b.email)).filter(|e| mailto_url(e).is_some()).unwrap_or_default(),
        websites: many(&b.websites).into_iter().filter(|w| normalize_url(w).is_some()).collect(),
        hours: many(&b.hours),
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct MemberView {
    pub jid: String,
    pub name: String,
    /// Formatted for display.
    pub phone: String,
    /// Raw digits, kept for search.
    pub phone_digits: String,
    /// "", "Admin" or "Group admin".
    pub badge: &'static str,
    pub is_me: bool,
    pub is_admin: bool,
    pub is_super_admin: bool,
    /// Menu entries of the row for the viewer (see `member_menu`).
    pub can_promote: bool,
    pub can_demote: bool,
    pub can_remove: bool,
    /// Address the member's picture is stored and requested under.
    pub avatar_key: String,
}

/// Where a member's picture lives: phone-number addresses are what the chat
/// list uses, so a LID member maps to its phone number when that is known.
/// Device suffixes are dropped. Anything else keeps its own address.
pub(super) fn avatar_key(jid: &str, phone_digits: &str) -> String {
    let server = jid.split_once('@').map_or("", |(_, s)| s);
    match server {
        "lid" if !phone_digits.is_empty() => format!("{phone_digits}@s.whatsapp.net"),
        "s.whatsapp.net" | "lid" => format!("{}@{server}", jid_user(jid)),
        _ => jid.to_string(),
    }
}

/// Splits the pictures still to ask for into the first `first` (asked now) and
/// the rest (asked in batches later), in list order, without repeats. At most
/// `max_total` pictures are planned, so a huge group cannot flood the engine.
pub(super) fn request_plan(
    members: &[MemberView],
    known: impl Fn(&MemberView) -> bool,
    first: usize,
    max_total: usize,
) -> (Vec<String>, VecDeque<String>) {
    let mut seen = HashSet::new();
    let mut keys: Vec<String> = members
        .iter()
        .filter(|m| !known(m))
        .filter(|m| seen.insert(m.avatar_key.clone()))
        .map(|m| m.avatar_key.clone())
        .collect();
    keys.truncate(max_total);
    let later = keys.split_off(first.min(keys.len()));
    (keys, later.into())
}

/// Pops up to `n` entries of `queue` that are still `wanted`.
pub(super) fn take_batch(queue: &mut VecDeque<String>, n: usize, wanted: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    while out.len() < n {
        let Some(key) = queue.pop_front() else { break };
        if wanted(&key) {
            out.push(key);
        }
    }
    out
}

/// Country calling codes that are three digits long and common enough to matter.
const THREE_DIGIT_CODES: &[&str] = &[
    "212", "213", "216", "218", "234", "254", "351", "352", "353", "358", "380", "420", "961",
    "962", "964", "965", "966", "968", "970", "971", "972", "973", "974",
];

fn country_code_len(digits: &str) -> usize {
    if digits.starts_with('1') || digits.starts_with('7') {
        1
    } else if digits
        .get(..3)
        .is_some_and(|p| THREE_DIGIT_CODES.contains(&p))
    {
        3
    } else {
        2
    }
}

/// Sizes of the leading groups of the national number; the rest is one last group.
fn national_groups(cc: &str) -> &'static [usize] {
    match cc {
        "44" => &[4],
        "49" => &[3, 4],
        "971" => &[2, 3],
        _ => &[3, 3],
    }
}

/// "989123456789" -> "+98 912 345 6789". Simple per-country grouping, not a
/// full numbering-plan implementation. Input that has no digits is returned
/// unchanged.
pub(super) fn format_phone(e164: &str) -> String {
    let digits: String = e164.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return e164.trim().to_string();
    }
    if digits.len() < 7 {
        return format!("+{digits}");
    }
    let cc_len = country_code_len(&digits);
    let (cc, mut rest) = digits.split_at(cc_len);
    let mut out = format!("+{cc}");
    for &size in national_groups(cc) {
        if rest.len() <= size {
            break;
        }
        let (head, tail) = rest.split_at(size);
        out.push(' ');
        out.push_str(head);
        rest = tail;
    }
    if !rest.is_empty() {
        out.push(' ');
        out.push_str(rest);
    }
    out
}

/// "98912...@s.whatsapp.net" / "98912...:7@s.whatsapp.net" -> "98912...".
fn jid_user(jid: &str) -> &str {
    let user = jid.split('@').next().unwrap_or(jid);
    user.split(':').next().unwrap_or(user)
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// name -> push name -> formatted phone -> jid user part.
fn display_name(
    name: &Option<String>,
    push_name: &Option<String>,
    phone: &Option<String>,
    jid: &str,
) -> String {
    if let Some(n) = non_empty(name).or_else(|| non_empty(push_name)) {
        return n.to_string();
    }
    if let Some(p) = non_empty(phone) {
        return format_phone(p);
    }
    jid_user(jid).to_string()
}

/// "today", "yesterday" or "12 Mar 2024" in local time.
fn date_label(ts: i64, now: i64) -> Option<String> {
    let d: DateTime<Local> = Local.timestamp_opt(ts, 0).single()?;
    let n: DateTime<Local> = Local.timestamp_opt(now, 0).single()?;
    let days = n
        .date_naive()
        .signed_duration_since(d.date_naive())
        .num_days();
    Some(match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        _ => format!("{} {}", d.day(), d.format("%b %Y")),
    })
}

fn about_date_label(ts: i64, now: i64) -> String {
    match date_label(ts, now) {
        Some(l) if l == "today" || l == "yesterday" => format!("Set {l}"),
        Some(l) => format!("Set on {l}"),
        None => String::new(),
    }
}

fn member_view(m: &GroupMember) -> MemberView {
    let name = display_name(&m.name, &None, &m.phone, &m.jid);
    let badge = if m.is_super_admin {
        "Group admin"
    } else if m.is_admin {
        "Admin"
    } else {
        ""
    };
    let phone_digits: String = m
        .phone
        .as_deref()
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    MemberView {
        jid: m.jid.clone(),
        name,
        phone: non_empty(&m.phone).map(format_phone).unwrap_or_default(),
        avatar_key: avatar_key(&m.jid, &phone_digits),
        phone_digits,
        badge,
        is_me: m.is_me,
        is_admin: m.is_admin || m.is_super_admin,
        is_super_admin: m.is_super_admin,
        ..MemberView::default()
    }
}

/// You first, then admins, then everyone else; each block by name.
pub(super) fn sort_members(members: &mut [MemberView]) {
    members.sort_by_cached_key(|m| (!m.is_me, !m.is_admin, m.name.to_lowercase()));
}

/// Case-insensitive match on name or phone. A query with digits also matches
/// the raw phone number, so "+98 912" finds "989123456789".
pub(super) fn filter_members(members: &[MemberView], query: &str) -> Vec<MemberView> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return members.to_vec();
    }
    let q_digits: String = q.chars().filter(char::is_ascii_digit).collect();
    members
        .iter()
        .filter(|m| {
            (m.is_me && "you".contains(&q))
                || m.name.to_lowercase().contains(&q)
                || m.phone.to_lowercase().contains(&q)
                || (!q_digits.is_empty() && m.phone_digits.contains(&q_digits))
        })
        .cloned()
        .collect()
}

fn created_label(
    created_at: Option<i64>,
    creator_jid: Option<&str>,
    members: &[GroupMember],
    now: i64,
) -> String {
    let Some(date) = created_at.and_then(|ts| date_label(ts, now)) else {
        return String::new();
    };
    let creator = creator_jid.filter(|j| !j.is_empty()).map(|jid| {
        let user = jid_user(jid);
        match members.iter().find(|m| jid_user(&m.jid) == user) {
            Some(m) if m.is_me => "you".to_string(),
            Some(m) => member_view(m).name,
            None => format_phone(user),
        }
    });
    match creator {
        Some(c) => format!("Created by {c}, {date}"),
        None => format!("Created {date}"),
    }
}

/// A document row of the "Media, links and docs" view.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DocView {
    pub id: String,
    pub name: String,
    pub size: String,
    pub date: String,
    /// Upper-case extension for the icon ("PDF"); empty when there is none.
    pub ext: String,
}

/// A link found in a message.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct LinkView {
    pub url: String,
    pub domain: String,
    pub date: String,
}

/// "Today", "Yesterday" or "12 Mar 2024" in local time.
pub(super) fn item_date(ts: i64, now: i64) -> String {
    match date_label(ts, now) {
        Some(l) if l == "today" => "Today".to_string(),
        Some(l) if l == "yesterday" => "Yesterday".to_string(),
        Some(l) => l,
        None => String::new(),
    }
}

pub(super) fn size_label(bytes: Option<i64>) -> String {
    let Some(b) = bytes.filter(|b| *b > 0) else { return String::new() };
    let f = b as f64;
    if f >= 1024.0 * 1024.0 {
        format!("{:.1} MB", f / (1024.0 * 1024.0))
    } else if f >= 1024.0 {
        format!("{:.0} KB", f / 1024.0)
    } else {
        format!("{b} B")
    }
}

pub(super) fn doc_view(m: &MessageRow, now: i64) -> DocView {
    let name = m
        .media_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Document")
        .to_string();
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty() && e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_uppercase)
        .unwrap_or_default();
    DocView { id: m.id.clone(), name, size: size_label(m.media_size), date: item_date(m.ts, now), ext }
}

/// "https://www.example.com/a?b" -> "example.com".
pub(super) fn link_domain(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// Every web link of `rows` (newest message first), one entry per link.
/// E-mail addresses are not part of the shared-links list.
pub(super) fn link_views(rows: &[MessageRow], now: i64) -> Vec<LinkView> {
    rows.iter()
        .flat_map(|m| {
            let text = m.text.as_deref().unwrap_or_default();
            links::find_links(text)
                .into_iter()
                .filter(|(_, _, url)| !url.starts_with("mailto:"))
                .map(|(_, _, url)| LinkView { domain: link_domain(&url), url, date: item_date(m.ts, now) })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A website as typed by a business ("shop.example.com") becomes a link the
/// opener accepts. Only `host[:port][/path]` with an optional http(s) scheme
/// passes: an authority carrying userinfo ("google.com@evil.com") would show
/// one host and open another. `mailto:` links keep a plain address only.
pub(super) fn normalize_url(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\') {
        return None;
    }
    if t.len() >= 7 && t[..7].eq_ignore_ascii_case("mailto:") {
        return mailto_url(&t[7..]);
    }
    let lower = t.to_ascii_lowercase();
    let url = if lower.starts_with("http://") || lower.starts_with("https://") {
        t.to_string()
    } else {
        format!("https://{t}")
    };
    let rest = &url[url.find("://")? + 3..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    valid_authority(authority).then_some(url)
}

/// `host` or `host:port`, no userinfo, no brackets.
fn valid_authority(authority: &str) -> bool {
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    valid_domain(host)
}

fn valid_domain(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// "mailto:<addr>" for a plain `local@domain` address, never with a query.
pub(super) fn mailto_url(raw: &str) -> Option<String> {
    let addr = raw.trim();
    let (local, domain) = addr.split_once('@')?;
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && local.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    (addr.len() <= 254 && local_ok && domain.contains('.') && valid_domain(domain)).then(|| format!("mailto:{addr}"))
}

/// The host a website string really opens, for display next to it.
pub(super) fn website_host(raw: &str) -> Option<String> {
    normalize_url(raw).map(|u| link_domain(&u))
}

pub(super) fn to_model(info: &ContactInfo, now: i64) -> ContactInfoView {
    let name = display_name(&info.name, &info.push_name, &info.phone, &info.jid);
    let phone = non_empty(&info.phone).map(format_phone).unwrap_or_default();
    let mut view = ContactInfoView {
        jid: info.jid.clone(),
        initials: initials(&name),
        tint_key: info.jid.clone(),
        is_business: info.is_business,
        loading: !info.complete,
        picture_path: info.picture_path.clone(),
        ..Default::default()
    };
    match &info.group {
        Some(g) => {
            let subject = g.subject.trim();
            let title = if subject.is_empty() {
                name
            } else {
                subject.to_string()
            };
            view.initials = initials(&title);
            view.subtitle = match g.participants.len() {
                1 => "Group · 1 member".to_string(),
                n => format!("Group · {n} members"),
            };
            view.title = title.clone();
            view.name = title;
            view.is_group = true;
            view.description = g
                .description
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            view.description_long =
                view.description.chars().count() > 140 || view.description.contains('\n');
            view.created_label =
                created_label(g.created_at, g.creator_jid.as_deref(), &g.participants, now);
            view.controls = group_controls(g, info.complete);
            let mut members: Vec<MemberView> = g.participants.iter().map(member_view).collect();
            for m in &mut members {
                (m.can_promote, m.can_demote, m.can_remove) = member_menu(view.controls.is_admin, m);
            }
            sort_members(&mut members);
            view.members = members;
        }
        None => {
            view.title = name.clone();
            view.name = name;
            view.phone = phone;
            if info.is_business {
                view.business = info.business.as_ref().map(business_view).unwrap_or_default();
            }
            view.about = non_empty(&info.about).unwrap_or_default().to_string();
            view.about_date = match (view.about.is_empty(), info.about_set_at) {
                (false, Some(ts)) => about_date_label(ts, now),
                _ => String::new(),
            };
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_engine::GroupInfo;

    fn ts(y: i32, m: u32, d: u32) -> i64 {
        Local
            .with_ymd_and_hms(y, m, d, 12, 0, 0)
            .single()
            .map(|t| t.timestamp())
            .unwrap_or_default()
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    fn member(jid: &str, name: Option<&str>, phone: Option<&str>) -> GroupMember {
        GroupMember {
            jid: jid.into(),
            name: name.map(String::from),
            phone: phone.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn format_phone_groups_by_country() {
        assert_eq!(format_phone("989123456789"), "+98 912 345 6789");
        assert_eq!(format_phone("14155552671"), "+1 415 555 2671");
        assert_eq!(format_phone("447911123456"), "+44 7911 123456");
        assert_eq!(format_phone("4915123456789"), "+49 151 2345 6789");
        assert_eq!(format_phone("971501234567"), "+971 50 123 4567");
    }

    #[test]
    fn format_phone_edge_cases() {
        assert_eq!(format_phone("+98 912 345 6789"), "+98 912 345 6789");
        assert_eq!(format_phone("12345"), "+12345");
        assert_eq!(format_phone(""), "");
        assert_eq!(format_phone("abc"), "abc");
    }

    #[test]
    fn display_name_precedence() {
        let mut info = ContactInfo {
            jid: "989123456789@s.whatsapp.net".into(),
            name: some("Ali"),
            push_name: some("Ali P"),
            phone: some("989123456789"),
            ..Default::default()
        };
        assert_eq!(to_model(&info, 0).title, "Ali");
        info.name = some("  ");
        assert_eq!(to_model(&info, 0).title, "Ali P");
        info.push_name = None;
        assert_eq!(to_model(&info, 0).title, "+98 912 345 6789");
        info.phone = None;
        assert_eq!(to_model(&info, 0).title, "989123456789");
    }

    #[test]
    fn jid_user_strips_device_and_server() {
        assert_eq!(jid_user("123:7@s.whatsapp.net"), "123");
        assert_eq!(jid_user("123"), "123");
    }

    #[test]
    fn user_view_has_phone_about_and_loading() {
        let now = ts(2024, 3, 20);
        let info = ContactInfo {
            jid: "989123456789@s.whatsapp.net".into(),
            name: some("Ali"),
            phone: some("989123456789"),
            about: some("Busy"),
            about_set_at: Some(ts(2024, 3, 12)),
            is_business: true,
            complete: false,
            ..Default::default()
        };
        let v = to_model(&info, now);
        assert_eq!(v.phone, "+98 912 345 6789");
        assert_eq!(v.about, "Busy");
        assert_eq!(v.about_date, "Set on 12 Mar 2024");
        assert!(v.is_business && v.loading && !v.is_group);
        assert_eq!(v.initials, "A");
    }

    #[test]
    fn about_date_relative() {
        let now = ts(2024, 3, 20);
        assert_eq!(about_date_label(ts(2024, 3, 20), now), "Set today");
        assert_eq!(about_date_label(ts(2024, 3, 19), now), "Set yesterday");
    }

    #[test]
    fn members_sorted_you_then_admins_then_name() {
        let mut me = member("1@s.whatsapp.net", Some("Zed"), None);
        me.is_me = true;
        let mut admin = member("2@s.whatsapp.net", Some("Yara"), None);
        admin.is_admin = true;
        let mut sa = member("3@s.whatsapp.net", Some("Xavier"), None);
        sa.is_super_admin = true;
        let list = vec![
            member("4@s.whatsapp.net", Some("bob"), None),
            member("5@s.whatsapp.net", Some("Alice"), None),
            admin,
            me,
            sa,
        ];
        let info = ContactInfo {
            jid: "g@g.us".into(),
            group: Some(GroupInfo {
                subject: "Team".into(),
                participants: list,
                ..Default::default()
            }),
            complete: true,
            ..Default::default()
        };
        let v = to_model(&info, 0);
        let names: Vec<&str> = v.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Zed", "Xavier", "Yara", "Alice", "bob"]);
        assert_eq!(v.members[1].badge, "Group admin");
        assert_eq!(v.members[2].badge, "Admin");
        assert_eq!(v.subtitle, "Group · 5 members");
        assert_eq!(v.title, "Team");
        assert!(!v.loading);
    }

    fn group_info(admin: bool, member: bool, locked: bool) -> GroupInfo {
        GroupInfo { subject: "Team".into(), i_am_admin: admin, i_am_member: member, locked, ..Default::default() }
    }

    fn controls(admin: bool, member: bool, locked: bool, complete: bool) -> GroupControls {
        group_controls(&group_info(admin, member, locked), complete)
    }

    #[test]
    fn admins_get_every_control() {
        let c = controls(true, true, true, true);
        assert!(c.is_admin && c.can_edit_info && c.can_leave && !c.left);
    }

    #[test]
    fn plain_members_edit_info_only_while_the_group_is_unlocked() {
        let open = controls(false, true, false, true);
        assert!(!open.is_admin && open.can_edit_info && open.can_leave);
        let locked = controls(false, true, true, true);
        assert!(!locked.is_admin && !locked.can_edit_info && locked.can_leave);
        assert!(locked.locked);
    }

    #[test]
    fn someone_who_left_sees_no_controls_but_the_notice() {
        let c = controls(true, false, false, true);
        assert!(c.left);
        assert!(!c.is_admin && !c.can_edit_info && !c.can_leave);
    }

    #[test]
    fn pending_requests_are_for_admins_of_approving_groups() {
        let mut g = group_info(true, true, false);
        g.pending_requests = 4;
        assert_eq!(group_controls(&g, true).pending_requests, 0, "approval is off");
        g.approval_required = true;
        assert_eq!(group_controls(&g, true).pending_requests, 4);
        assert_eq!(group_controls(&g, false).pending_requests, 0, "not before the answer is complete");
        g.i_am_admin = false;
        assert_eq!(group_controls(&g, true).pending_requests, 0);
    }

    #[test]
    fn nothing_is_offered_before_the_answer_is_complete() {
        let mut g = group_info(true, true, false);
        g.announce_only = true;
        g.approval_required = true;
        let c = group_controls(&g, false);
        assert!(!c.is_admin && !c.can_edit_info && !c.can_leave && !c.left);
        // The settings values still reflect what is known.
        assert!(c.announce_only && c.approval_required && !c.locked);
        assert!(!controls(false, false, false, false).left);
    }

    #[test]
    fn member_menu_follows_the_viewer_and_the_target() {
        let target = |admin: bool, sa: bool, me: bool| MemberView { is_admin: admin || sa, is_super_admin: sa, is_me: me, ..Default::default() };
        assert_eq!(member_menu(true, &target(false, false, false)), (true, false, true));
        assert_eq!(member_menu(true, &target(true, false, false)), (false, true, true));
        // The creator can be neither demoted nor removed.
        assert_eq!(member_menu(true, &target(true, true, false)), (false, false, false));
        assert_eq!(member_menu(true, &target(false, false, true)), (false, false, false));
        assert_eq!(member_menu(false, &target(false, false, false)), (false, false, false));
    }

    #[test]
    fn member_rows_carry_the_menu_for_admin_viewers_only() {
        let mut me = member("1@s.whatsapp.net", Some("Me"), None);
        me.is_me = true;
        me.is_admin = true;
        let mut creator = member("2@s.whatsapp.net", Some("Boss"), None);
        creator.is_super_admin = true;
        let list = vec![me, creator, member("3@s.whatsapp.net", Some("Cy"), None)];
        let mut info = ContactInfo {
            jid: "g@g.us".into(),
            group: Some(GroupInfo { participants: list, i_am_admin: true, i_am_member: true, ..Default::default() }),
            complete: true,
            ..Default::default()
        };
        let row = |v: &ContactInfoView, name: &str| v.members.iter().find(|m| m.name == name).map(|m| (m.can_promote, m.can_demote, m.can_remove)).unwrap_or_default();
        let v = to_model(&info, 0);
        assert_eq!(row(&v, "Me"), (false, false, false));
        assert_eq!(row(&v, "Boss"), (false, false, false));
        assert_eq!(row(&v, "Cy"), (true, false, true));
        info.complete = false;
        assert_eq!(row(&to_model(&info, 0), "Cy"), (false, false, false));
        if let Some(g) = info.group.as_mut() {
            g.i_am_admin = false;
        }
        info.complete = true;
        assert_eq!(row(&to_model(&info, 0), "Cy"), (false, false, false));
    }

    #[test]
    fn member_filter_is_case_insensitive_on_name_and_phone() {
        let members: Vec<MemberView> = [
            member("1@s.whatsapp.net", Some("Alice"), Some("989123456789")),
            member("2@s.whatsapp.net", Some("Bob"), Some("14155552671")),
        ]
        .iter()
        .map(member_view)
        .collect();
        assert_eq!(filter_members(&members, "ALI").len(), 1);
        assert_eq!(filter_members(&members, "").len(), 2);
        assert_eq!(filter_members(&members, "+1 415")[0].name, "Bob");
        assert_eq!(filter_members(&members, "98912")[0].name, "Alice");
        assert!(filter_members(&members, "zzz").is_empty());
    }

    #[test]
    fn created_label_names_creator() {
        let now = ts(2024, 3, 20);
        let mut me = member("1@s.whatsapp.net", Some("Me"), None);
        me.is_me = true;
        let ali = member("2:5@s.whatsapp.net", Some("Ali"), None);
        let members = vec![me, ali];
        let d = Some(ts(2024, 1, 5));
        assert_eq!(
            created_label(d, Some("2@s.whatsapp.net"), &members, now),
            "Created by Ali, 5 Jan 2024"
        );
        assert_eq!(
            created_label(d, Some("1@s.whatsapp.net"), &members, now),
            "Created by you, 5 Jan 2024"
        );
        assert_eq!(created_label(d, None, &members, now), "Created 5 Jan 2024");
        assert_eq!(created_label(None, None, &members, now), "");
    }

    #[test]
    fn business_view_drops_empty_fields() {
        let b = BusinessProfile {
            description: some("  "),
            category: some("Retail"),
            address: None,
            email: some("a@b.co"),
            websites: vec!["shop.example.com".into(), " ".into()],
            hours: vec![],
        };
        let v = business_view(&b);
        assert_eq!(v.category, "Retail");
        assert_eq!(v.description, "");
        assert_eq!(v.websites, ["shop.example.com"]);
        assert!(!v.is_empty());
        assert!(business_view(&BusinessProfile::default()).is_empty());
    }

    #[test]
    fn business_details_only_for_business_people() {
        let mut info = ContactInfo {
            jid: "1@s.whatsapp.net".into(),
            is_business: true,
            business: Some(BusinessProfile { category: some("Retail"), ..Default::default() }),
            complete: true,
            ..Default::default()
        };
        assert_eq!(to_model(&info, 0).business.category, "Retail");
        info.is_business = false;
        assert!(to_model(&info, 0).business.is_empty());
    }

    #[test]
    fn link_domain_strips_scheme_www_and_path() {
        assert_eq!(link_domain("https://www.example.com/a?b=1"), "example.com");
        assert_eq!(link_domain("http://user@a.io:8080#x"), "a.io:8080");
        assert_eq!(link_domain("sub.example.org"), "sub.example.org");
    }

    #[test]
    fn normalize_url_adds_a_scheme_only_when_missing() {
        assert_eq!(normalize_url(" shop.example.com ").as_deref(), Some("https://shop.example.com"));
        assert_eq!(normalize_url("http://a.io").as_deref(), Some("http://a.io"));
        assert_eq!(normalize_url("HTTPS://a.io:8080/x").as_deref(), Some("HTTPS://a.io:8080/x"));
        assert_eq!(normalize_url("mailto:a@b.co").as_deref(), Some("mailto:a@b.co"));
        assert_eq!(normalize_url("x.com/?u=http://y").as_deref(), Some("https://x.com/?u=http://y"));
    }

    #[test]
    fn normalize_url_rejects_spoofed_or_malformed_hosts() {
        for bad in [
            "google.com@evil.com",
            "https://user:pw@x.com",
            "http://google.com@evil.com/path",
            "ftp://x.com",
            "javascript:alert(1)",
            "https://",
            "exa mple.com",
            "x.com:port",
            "https://[::1]/",
            "https://a.com\\@evil.com",
            "",
        ] {
            assert_eq!(normalize_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn mailto_accepts_only_a_plain_address() {
        assert_eq!(normalize_url("mailto:hello@clay.example").as_deref(), Some("mailto:hello@clay.example"));
        assert_eq!(mailto_url("john.doe+x@mail.example.org").as_deref(), Some("mailto:john.doe+x@mail.example.org"));
        for bad in ["a@b.co?attach=x", "a@b.co&bcc=c@d.e", "a%0d@b.co", "a b@c.de", "a@b", "@b.co", "a@@b.co"] {
            assert_eq!(mailto_url(bad), None, "{bad}");
        }
        assert_eq!(normalize_url("mailto:a@b.co?attach=x"), None);
    }

    #[test]
    fn website_host_is_the_canonical_host() {
        assert_eq!(website_host("https://www.clay.example/gifts").as_deref(), Some("clay.example"));
        assert_eq!(website_host("google.com@evil.com"), None);
    }

    #[test]
    fn avatar_key_maps_lid_to_phone_number() {
        assert_eq!(avatar_key("123@s.whatsapp.net", "123"), "123@s.whatsapp.net");
        assert_eq!(avatar_key("123:7@s.whatsapp.net", ""), "123@s.whatsapp.net");
        assert_eq!(avatar_key("999@lid", "98912"), "98912@s.whatsapp.net");
        assert_eq!(avatar_key("999:3@lid", ""), "999@lid");
        assert_eq!(avatar_key("g@g.us", "1"), "g@g.us");
        let v = member_view(&member("999@lid", Some("Lia"), Some("+98 912")));
        assert_eq!(v.avatar_key, "98912@s.whatsapp.net");
    }

    fn keyed(keys: &[&str]) -> Vec<MemberView> {
        keys.iter().map(|k| MemberView { jid: (*k).into(), avatar_key: (*k).into(), ..Default::default() }).collect()
    }

    #[test]
    fn request_plan_asks_first_rows_now_and_queues_the_rest() {
        let members = keyed(&["a@s.whatsapp.net", "b@s.whatsapp.net", "c@s.whatsapp.net", "a@s.whatsapp.net", "d@s.whatsapp.net"]);
        let (now, later) = request_plan(&members, |m| m.jid == "b@s.whatsapp.net", 2, 100);
        assert_eq!(now, ["a@s.whatsapp.net", "c@s.whatsapp.net"]);
        assert_eq!(later, ["d@s.whatsapp.net"]);
        let (now, later) = request_plan(&members, |_| true, 2, 100);
        assert!(now.is_empty() && later.is_empty());
        let (now, later) = request_plan(&members, |_| false, 10, 100);
        assert_eq!(now.len(), 4);
        assert!(later.is_empty());
        let (now, later) = request_plan(&members, |_| false, 2, 3);
        assert_eq!((now.len(), later.len()), (2, 1));
        let (now, later) = request_plan(&members, |m| m.jid == "a@s.whatsapp.net", 10, 100);
        assert_eq!((now.len(), later.len()), (3, 0));
        let (now, later) = request_plan(&members, |_| false, 0, 0);
        assert!(now.is_empty() && later.is_empty());
    }

    #[test]
    fn take_batch_skips_unwanted_and_respects_the_size() {
        let mut q: VecDeque<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        assert_eq!(take_batch(&mut q, 2, |k| k != "a"), ["b", "c"]);
        assert_eq!(take_batch(&mut q, 10, |_| true), ["d", "e"]);
        assert!(take_batch(&mut q, 10, |_| true).is_empty());
    }

    #[test]
    fn size_label_units() {
        assert_eq!(size_label(None), "");
        assert_eq!(size_label(Some(0)), "");
        assert_eq!(size_label(Some(900)), "900 B");
        assert_eq!(size_label(Some(2048)), "2 KB");
        assert_eq!(size_label(Some(3 * 1024 * 1024)), "3.0 MB");
    }

    fn row(id: &str, ts: i64, text: Option<&str>, name: Option<&str>) -> MessageRow {
        MessageRow {
            chat_jid: "c".into(),
            id: id.into(),
            sender_jid: "c".into(),
            sender_name: None,
            from_me: false,
            ts,
            kind: wa_store::MessageKind::Text,
            text: text.map(String::from),
            quoted_id: None,
            quoted_preview: None,
            media_mime: None,
            media_name: name.map(String::from),
            media_size: Some(2048),
            media_duration: None,
            thumbnail: None,
            local_path: None,
            status: wa_store::MessageStatus::Read,
            revoked: false,
            edited: false,
            ..Default::default()
        }
    }

    #[test]
    fn link_views_list_every_web_link_but_no_emails() {
        let now = ts(2024, 3, 20);
        let rows = vec![
            row("1", ts(2024, 3, 20), Some("a https://www.example.com/x and www.b.org, mail me x@y.co"), None),
            row("2", ts(2024, 3, 19), Some("nothing"), None),
        ];
        let v = link_views(&rows, now);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].url, "https://www.example.com/x");
        assert_eq!(v[0].domain, "example.com");
        assert_eq!(v[0].date, "Today");
        assert_eq!(v[1].domain, "b.org");
    }

    #[test]
    fn doc_view_names_sizes_and_icons() {
        let now = ts(2024, 3, 20);
        let d = doc_view(&row("1", ts(2024, 3, 19), None, Some("Report.final.pdf")), now);
        assert_eq!(
            (d.name.as_str(), d.ext.as_str(), d.size.as_str(), d.date.as_str()),
            ("Report.final.pdf", "PDF", "2 KB", "Yesterday")
        );
        let d = doc_view(&row("2", ts(2024, 1, 5), None, None), now);
        assert_eq!((d.name.as_str(), d.ext.as_str(), d.date.as_str()), ("Document", "", "5 Jan 2024"));
    }

    struct NoopWake;
    impl std::task::Wake for NoopWake {
        fn wake(self: std::sync::Arc<Self>) {}
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        let waker = std::task::Waker::from(std::sync::Arc::new(NoopWake));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn contact_info_slint_compiles() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/contact_info.slint");
        let result = block_on(slint_interpreter::Compiler::default().build_from_path(path));
        let errors: Vec<String> = result
            .diagnostics()
            .filter(|d| d.level() == slint_interpreter::DiagnosticLevel::Error)
            .map(|d| d.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "contact_info.slint errors:
{}",
            errors.join(
                "
"
            )
        );
        assert!(result.component("ContactInfoPanel").is_some());
    }
}
