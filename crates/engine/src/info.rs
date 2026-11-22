//! Contact / group info panel: a quick reply from local data, then network
//! lookups (about, full-resolution picture, group metadata) that go through the
//! same pacing as the background avatar and group-subject fetches.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whatsapp_rust::http::HttpRequest;
use whatsapp_rust::prelude::{Client, Jid};
use whatsapp_rust::wacore::iq::business as biz;
use whatsapp_rust::wacore::types::message::AddressingMode;
use whatsapp_rust::wacore_binary::JidExt as _;
use whatsapp_rust::{GroupError, GroupMetadata, IqError};
use wa_store::{ChatKind, ContactRow};

use crate::api::{BusinessProfile, ContactInfo, GroupInfo, GroupMember, UiEvent};
use crate::avatar::{paced_slot, record_outcome};
use crate::events::{Shared, own_jid};
use crate::groups::store_subject;
use crate::jid::redact;
use crate::media::{avatar_full_path, lock, write_atomic_async};

/// The same jid is looked up over the network at most once per this window;
/// asking again inside it returns the previous result.
const INFO_TTL: Duration = Duration::from_secs(60);
/// How long fetched group metadata is reused (reopening the panel).
const GROUP_META_TTL: Duration = Duration::from_secs(5 * 60);

/// One group participant as reported by the server.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawMember {
    pub jid: String,
    /// Phone-number form, when the server told us (LID-addressed groups).
    pub pn: Option<String>,
    pub is_admin: bool,
    pub is_super_admin: bool,
}

/// Group metadata reduced to what the info panel shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GroupMeta {
    pub subject: String,
    pub description: Option<String>,
    pub created_at: Option<i64>,
    pub creator: Option<String>,
    pub members: Vec<RawMember>,
    pub addressing: AddressingMode,
    pub announce_only: bool,
    pub locked: bool,
    pub approval_required: bool,
}

/// Result of asking the cache whether a lookup may go to the network.
#[derive(Debug, PartialEq)]
pub(crate) enum Claim {
    /// A complete result from the last [`INFO_TTL`].
    Cached(Box<ContactInfo>),
    /// Another lookup of the same jid is running; it will emit the result.
    Busy,
    /// The caller owns the lookup; wrap it in a [`ClaimGuard`] so that an
    /// aborted task cannot leave the jid claimed forever.
    Go,
}

#[derive(Default)]
struct Inner {
    done: HashMap<String, (Instant, ContactInfo)>,
    /// Claimed jids with the generation they were claimed at.
    in_flight: HashMap<String, u64>,
    /// Bumped by `invalidate`; a lookup that started before a bump is stale.
    generations: HashMap<String, u64>,
    groups: HashMap<String, (Instant, GroupMeta)>,
    /// Jids asked for again while their lookup was running.
    dirty: HashSet<String>,
}

/// Per-generation memory of info lookups.
#[derive(Default)]
pub(crate) struct InfoCache {
    inner: Mutex<Inner>,
}

impl InfoCache {
    pub(crate) fn clear(&self) {
        *lock(&self.inner) = Inner::default();
    }

    /// Forgets the remembered result and group metadata of `jid` (its picture
    /// or group changed); a running lookup keeps its claim.
    pub(crate) fn invalidate(&self, jid: &str) {
        let mut inner = lock(&self.inner);
        inner.done.remove(jid);
        inner.groups.remove(jid);
        let generation = inner.generations.entry(jid.to_string()).or_insert(0);
        *generation += 1;
    }

    /// Gives up a claim without remembering a result, so the next open retries.
    fn release(&self, jid: &str) {
        lock(&self.inner).in_flight.remove(jid);
    }

    pub(crate) fn claim(&self, jid: &str, now: Instant) -> Claim {
        let mut inner = lock(&self.inner);
        inner
            .done
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < INFO_TTL);
        if let Some((_, info)) = inner.done.get(jid) {
            return Claim::Cached(Box::new(info.clone()));
        }
        if inner.in_flight.contains_key(jid) {
            inner.dirty.insert(jid.to_string());
            return Claim::Busy;
        }
        inner.dirty.remove(jid);
        let generation = inner.generations.get(jid).copied().unwrap_or(0);
        inner.in_flight.insert(jid.to_string(), generation);
        Claim::Go
    }

    /// After a lookup ended: if the jid was requested again meanwhile, claims
    /// it for one more lookup that ignores the remembered result (it may
    /// predate the change that prompted the request). Any number of requests
    /// made during the run coalesce into this single rerun.
    pub(crate) fn take_rerun(&self, jid: &str) -> bool {
        let mut inner = lock(&self.inner);
        if !inner.dirty.remove(jid) || inner.in_flight.contains_key(jid) {
            return false;
        }
        inner.done.remove(jid);
        let generation = inner.generations.get(jid).copied().unwrap_or(0);
        inner.in_flight.insert(jid.to_string(), generation);
        true
    }

    /// Ends a lookup started with [`Claim::Go`] and remembers its result,
    /// unless the jid was invalidated since the claim (the result predates the
    /// change).
    pub(crate) fn finish(&self, jid: &str, now: Instant, info: ContactInfo) {
        let mut inner = lock(&self.inner);
        let claimed = inner.in_flight.remove(jid);
        let current = inner.generations.get(jid).copied().unwrap_or(0);
        if claimed.is_some_and(|g| g != current) {
            return;
        }
        inner.done.insert(jid.to_string(), (now, info));
    }

    pub(crate) fn group_meta(&self, jid: &str, now: Instant) -> Option<GroupMeta> {
        let mut inner = lock(&self.inner);
        inner
            .groups
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < GROUP_META_TTL);
        inner.groups.get(jid).map(|(_, meta)| meta.clone())
    }

    /// Addressing mode of a group from the cache only; never touches the network.
    pub(crate) fn cached_addressing(&self, jid: &str, now: Instant) -> Option<AddressingMode> {
        self.group_meta(jid, now).map(|m| m.addressing)
    }

    pub(crate) fn put_group_meta(&self, jid: &str, now: Instant, meta: GroupMeta) {
        lock(&self.inner).groups.insert(jid.to_string(), (now, meta));
    }
}

/// Holds the claim of a running lookup. Dropping it (also when the task is
/// aborted) releases the claim without caching anything; only
/// [`ClaimGuard::finish`] remembers a result.
pub(crate) struct ClaimGuard {
    cache: Arc<InfoCache>,
    jid: String,
    armed: bool,
}

impl ClaimGuard {
    pub(crate) fn new(cache: Arc<InfoCache>, jid: String) -> Self {
        ClaimGuard { cache, jid, armed: true }
    }

    pub(crate) fn finish(mut self, now: Instant, info: ContactInfo) {
        self.armed = false;
        self.cache.finish(&self.jid, now, info);
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cache.release(&self.jid);
        }
    }
}

// ---------------------------------------------------------------- pure helpers

/// E.164 digits (no '+') of a phone-number JID; `None` for any other JID.
pub(crate) fn phone_of_jid(jid: &str) -> Option<String> {
    let user = jid.strip_suffix("@s.whatsapp.net")?;
    let user = user.split(':').next().unwrap_or(user);
    (!user.is_empty() && user.bytes().all(|b| b.is_ascii_digit())).then(|| user.to_string())
}

/// Address-book name, else the contact's own push name.
pub(crate) fn best_name(c: &ContactRow) -> Option<String> {
    [&c.full_name, &c.push_name]
        .into_iter()
        .flatten()
        .find(|n| !n.is_empty())
        .cloned()
}

fn nonempty(s: &Option<String>) -> Option<String> {
    s.clone().filter(|s| !s.is_empty())
}

/// The quick reply: everything known without the network. `picture` is an
/// existing cached picture of the jid (the preview one is fine here).
pub(crate) fn local_info(
    jid: &str,
    contact: Option<&ContactRow>,
    group_subject: Option<&str>,
    picture: Option<PathBuf>,
) -> ContactInfo {
    let group = jid.ends_with("@g.us").then(|| GroupInfo {
        subject: group_subject.unwrap_or_default().to_string(),
        // Unknown until the metadata arrives; do not claim we left.
        i_am_member: true,
        ..GroupInfo::default()
    });
    ContactInfo {
        jid: jid.to_string(),
        name: contact.and_then(|c| nonempty(&c.full_name)),
        push_name: contact.and_then(|c| nonempty(&c.push_name)),
        phone: contact
            .and_then(|c| nonempty(&c.phone))
            .or_else(|| phone_of_jid(jid)),
        about: contact.and_then(|c| nonempty(&c.about)),
        about_set_at: contact.and_then(|c| c.about.as_ref().and(c.about_set_at)),
        picture_path: picture,
        is_business: false,
        business: None,
        complete: false,
        group,
    }
}

/// Whether the local data alone already tells the user something.
fn has_content(info: &ContactInfo) -> bool {
    info.name.is_some()
        || info.push_name.is_some()
        || info.phone.is_some()
        || info.about.is_some()
        || info.picture_path.is_some()
        || info.group.as_ref().is_some_and(|g| !g.subject.is_empty())
}

/// Maps server participants to panel rows. `me` holds our own JIDs (phone and
/// LID form); `name_of` resolves a JID to an address-book / push name.
pub(crate) fn map_members(
    raw: &[RawMember],
    me: &[String],
    name_of: impl Fn(&str) -> Option<String>,
) -> Vec<GroupMember> {
    raw.iter()
        .map(|m| {
            // The store keys people by phone-number JID whenever it is known.
            let jid = m.pn.clone().unwrap_or_else(|| m.jid.clone());
            let phone = phone_of_jid(&jid).or_else(|| phone_of_jid(&m.jid));
            let name = name_of(&jid)
                .or_else(|| name_of(&m.jid))
                .or_else(|| phone.as_ref().map(|p| format!("+{p}")));
            GroupMember {
                is_me: me.iter().any(|own| *own == jid || *own == m.jid),
                jid,
                name,
                phone,
                is_admin: m.is_admin,
                is_super_admin: m.is_super_admin,
            }
        })
        .collect()
}

pub(crate) fn group_meta_from(meta: &GroupMetadata) -> GroupMeta {
    let bare = |j: &Jid| j.to_non_ad().to_string();
    GroupMeta {
        subject: meta.subject.clone(),
        description: meta.description.clone().filter(|d| !d.is_empty()),
        created_at: meta.creation_time.and_then(|t| i64::try_from(t).ok()),
        creator: meta
            .creator_pn
            .as_ref()
            .or(meta.creator.as_ref())
            .map(bare),
        members: meta
            .participants
            .iter()
            .map(|p| RawMember {
                jid: bare(&p.jid),
                pn: p.phone_number.as_ref().map(bare),
                is_admin: p.is_admin(),
                is_super_admin: p.is_super_admin(),
            })
            .collect(),
        addressing: meta.addressing_mode,
        announce_only: meta.is_announcement,
        locked: meta.is_locked,
        approval_required: meta.membership_approval,
    }
}

/// `(is_admin, is_member)` of us among the server's participants. `me` holds
/// our own JIDs in phone and LID form, as for [`map_members`].
pub(crate) fn my_membership(members: &[RawMember], me: &[String]) -> (bool, bool) {
    let mut member = false;
    let mut admin = false;
    for m in members {
        if me.iter().any(|own| *own == m.jid || m.pn.as_ref() == Some(own)) {
            member = true;
            admin |= m.is_admin || m.is_super_admin;
        }
    }
    (admin, member)
}

/// Whether a server error code on a group metadata query means "you are not
/// (or no longer) a participant": 401/403 (not authorized / forbidden) or 404
/// (gone). WhatsApp answers this way for a group we left or were removed from.
pub(crate) fn is_not_member_code(code: u16) -> bool {
    matches!(code, 401 | 403 | 404)
}

/// The server's numeric error code behind a group operation failure, if any.
pub(crate) fn server_code(e: &GroupError) -> Option<u16> {
    match e {
        GroupError::Iq(IqError::ServerError { code, .. }) => Some(*code),
        _ => None,
    }
}

fn group_info_from(
    meta: &GroupMeta,
    me: &[String],
    name_of: impl Fn(&str) -> Option<String>,
) -> GroupInfo {
    let (i_am_admin, i_am_member) = my_membership(&meta.members, me);
    GroupInfo {
        subject: meta.subject.clone(),
        description: meta.description.clone(),
        created_at: meta.created_at,
        creator_jid: meta.creator.clone(),
        participants: map_members(&meta.members, me, name_of),
        announce_only: meta.announce_only,
        locked: meta.locked,
        approval_required: meta.approval_required,
        i_am_admin,
        i_am_member,
    }
}

// --------------------------------------------------------------------- lookups

fn existing_file(path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(path);
    p.is_file().then_some(p)
}

fn load_local(sh: &Shared, jid: &str) -> ContactInfo {
    let contact = sh.store.contact(jid).unwrap_or_else(|e| {
        log::error!("contact lookup failed: {e}");
        None
    });
    let subject = (ChatKind::from_jid(jid) == ChatKind::Group)
        .then(|| sh.store.chat(jid))
        .and_then(|r| {
            r.unwrap_or_else(|e| {
                log::error!("chat lookup failed: {e}");
                None
            })
        })
        .map(|c| c.display_name)
        .filter(|n| n != jid);
    let picture = contact
        .as_ref()
        .and_then(|c| c.avatar_path.as_deref())
        .and_then(existing_file);
    local_info(jid, contact.as_ref(), subject.as_deref(), picture)
}

fn store_name_of(sh: &Shared) -> impl Fn(&str) -> Option<String> + '_ {
    |jid| {
        sh.store
            .contact(jid)
            .unwrap_or_else(|e| {
                log::error!("contact lookup failed: {e}");
                None
            })
            .as_ref()
            .and_then(best_name)
    }
}

/// Downloads the full-resolution picture unless a file for the same picture id
/// is cached. `known_id` (from usync or the stored preview) lets a cached file
/// answer without any request. `Ok(None)`: the jid has no visible picture.
pub(crate) async fn full_picture(
    sh: &Shared,
    client: &Client,
    jid: &Jid,
    jid_str: &str,
    known_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(id) = known_id {
        let cached = avatar_full_path(&sh.media.dir, jid_str, id);
        if cached.is_file() {
            return Ok(Some(cached));
        }
    }
    let Some(_slot) = paced_slot(sh).await else {
        anyhow::bail!("picture requests are paused");
    };
    let pic = match client.contacts().get_profile_picture(jid, false).await {
        Ok(pic) => {
            record_outcome(sh, true);
            pic
        }
        Err(e) => {
            record_outcome(sh, false);
            return Err(e.into());
        }
    };
    let Some(pic) = pic else {
        return Ok(None);
    };
    let path = avatar_full_path(&sh.media.dir, jid_str, &pic.id);
    if path.is_file() {
        return Ok(Some(path));
    }
    let response = client.http_client.execute(HttpRequest::get(pic.url)).await?;
    if response.status_code != 200 {
        anyhow::bail!("picture download answered HTTP {}", response.status_code);
    }
    write_atomic_async(path.clone(), response.body).await?;
    Ok(Some(path))
}

/// Network part for one user. Returns whether anything could be loaded.
async fn enrich_user(sh: &Shared, client: &Client, jid: &Jid, jid_str: &str, info: &mut ContactInfo) -> bool {
    let mut loaded = false;
    if info.phone.is_none()
        && jid.is_lid()
        && let Ok(Some(entry)) = client.get_lid_pn_entry(&jid.to_non_ad()).await
    {
        info.phone = Some(entry.phone_number.to_string());
        loaded = true;
    }

    let mut picture_id = sh
        .store
        .contact(jid_str)
        .ok()
        .flatten()
        .and_then(|c| c.avatar_id);
    // `None`: the lookup failed, so the picture is still tried below.
    let mut has_picture = None;
    if let Some(_slot) = paced_slot(sh).await {
        let result = client
            .contacts()
            .get_user_info(std::slice::from_ref(&jid.to_non_ad()))
            .await;
        record_outcome(sh, result.is_ok());
        match result {
            Ok(map) => {
                loaded = true;
                if let Some(user) = map.into_values().next() {
                    info.is_business = user.is_business;
                    if user.status_error.is_none() {
                        let about = user.status.filter(|s| !s.is_empty());
                        if about != info.about {
                            info.about_set_at = None;
                        }
                        info.about = about;
                        if let Err(e) = sh.store.set_contact_about(
                            jid_str,
                            info.about.as_deref(),
                            info.about_set_at,
                        ) {
                            log::error!("storing about failed: {e}");
                        }
                    }
                    has_picture = Some(user.picture_id.is_some());
                    picture_id = user.picture_id.or(picture_id);
                }
            }
            Err(e) => crate::jid::warn_lib_error(&format!("user info lookup for {} failed", redact(jid_str)), &e),
        }
    } else {
        log::debug!("user info for {} skipped: requests paused", redact(jid_str));
    }

    if info.is_business {
        info.business = business_profile(sh, client, jid, jid_str).await;
        loaded |= info.business.is_some();
    }

    if has_picture != Some(false) {
        match full_picture(sh, client, jid, jid_str, picture_id.as_deref()).await {
            Ok(path) => {
                loaded = true;
                if path.is_some() {
                    info.picture_path = path;
                }
            }
            Err(e) => log::warn!("picture lookup for {} failed: {e:#}", redact(jid_str)),
        }
    }
    loaded
}

/// The WhatsApp Business profile of `jid`, through the same pacing as the
/// other lookups. `None` when requests are paused, the query failed or the
/// account has no profile.
async fn business_profile(sh: &Shared, client: &Client, jid: &Jid, jid_str: &str) -> Option<BusinessProfile> {
    let _slot = paced_slot(sh).await?;
    let result = client.get_business_profile(&jid.to_non_ad()).await;
    record_outcome(sh, result.is_ok());
    match result {
        Ok(profile) => profile.as_ref().map(business_from),
        Err(e) => {
            crate::jid::warn_lib_error(&format!("business profile lookup for {} failed", redact(jid_str)), &e);
            None
        }
    }
}

fn day_label(day: &biz::DayOfWeek) -> String {
    match day {
        biz::DayOfWeek::Sunday => "Sun".into(),
        biz::DayOfWeek::Monday => "Mon".into(),
        biz::DayOfWeek::Tuesday => "Tue".into(),
        biz::DayOfWeek::Wednesday => "Wed".into(),
        biz::DayOfWeek::Thursday => "Thu".into(),
        biz::DayOfWeek::Friday => "Fri".into(),
        biz::DayOfWeek::Saturday => "Sat".into(),
        biz::DayOfWeek::Other(s) => s.clone(),
    }
}

/// "HH:MM" of a minutes-since-midnight value (24:00 shows as 00:00).
fn clock(minutes: u32) -> String {
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
}

/// One opening-hours line, e.g. "Mon: 09:00\u{2013}17:00".
pub(crate) fn hours_line(
    day: &biz::DayOfWeek,
    mode: &biz::BusinessHourMode,
    open: Option<u32>,
    close: Option<u32>,
) -> String {
    let what = match (mode, open, close) {
        (biz::BusinessHourMode::Open24H, _, _) => "Open 24 hours".to_string(),
        (biz::BusinessHourMode::AppointmentOnly, _, _) => "By appointment only".to_string(),
        (_, Some(o), Some(c)) => format!("{}\u{2013}{}", clock(o), clock(c)),
        (biz::BusinessHourMode::SpecificHours, _, _) => "Open".to_string(),
        (biz::BusinessHourMode::Other(m), _, _) => m.clone(),
    };
    format!("{}: {what}", day_label(day))
}

/// Maps the server's business profile to what the info panel shows. Empty
/// strings count as absent.
pub(crate) fn business_from(p: &biz::BusinessProfile) -> BusinessProfile {
    let non_empty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    let categories: Vec<&str> = p.categories.iter().map(|c| c.name.trim()).filter(|n| !n.is_empty()).collect();
    BusinessProfile {
        description: non_empty(&p.description),
        category: (!categories.is_empty()).then(|| categories.join(", ")),
        address: p.address.as_deref().and_then(non_empty),
        email: p.email.as_deref().and_then(non_empty),
        websites: p.website.iter().filter_map(|w| non_empty(w)).collect(),
        hours: p
            .business_hours
            .business_config
            .iter()
            .flatten()
            .map(|c| hours_line(&c.day_of_week, &c.mode, c.open_time, c.close_time))
            .collect(),
    }
}

/// Outcome of a group metadata lookup.
pub(crate) enum MetaLookup {
    Found(GroupMeta),
    /// The server says we are not a participant (left or removed).
    NotMember,
    /// Paused or failed for another reason.
    Unavailable,
}

/// Group metadata from the cache, else from the server through the paced
/// metadata slot.
pub(crate) async fn group_meta_lookup(sh: &Shared, client: &Client, jid: &Jid, jid_str: &str) -> MetaLookup {
    if let Some(meta) = sh.info.group_meta(jid_str, Instant::now()) {
        return MetaLookup::Found(meta);
    }
    let Some(_slot) = sh.groups.metadata_slot().await else {
        return MetaLookup::Unavailable;
    };
    match client.groups().get_metadata(jid).await {
        Ok(raw) => {
            let meta = group_meta_from(&raw);
            sh.info.put_group_meta(jid_str, Instant::now(), meta.clone());
            store_subject(sh, jid_str, &meta.subject);
            MetaLookup::Found(meta)
        }
        Err(e) => {
            crate::jid::warn_lib_error(&format!("group metadata for {} failed", redact(jid_str)), &e);
            if server_code(&e).is_some_and(is_not_member_code) {
                MetaLookup::NotMember
            } else {
                MetaLookup::Unavailable
            }
        }
    }
}

async fn group_meta(sh: &Shared, client: &Client, jid: &Jid, jid_str: &str) -> Option<GroupMeta> {
    match group_meta_lookup(sh, client, jid, jid_str).await {
        MetaLookup::Found(meta) => Some(meta),
        MetaLookup::NotMember | MetaLookup::Unavailable => None,
    }
}

/// Our own JIDs (phone and LID form) for comparing against participants.
pub(crate) fn own_jids(client: &Client) -> Vec<String> {
    own_jid(client)
        .into_iter()
        .chain(client.lid().map(|j| j.to_non_ad().to_string()))
        .collect()
}

/// Like [`group_addressing`] but from the cache only: no network, no slot.
pub(crate) fn cached_group_addressing(sh: &Shared, jid: &Jid) -> Option<AddressingMode> {
    sh.info.cached_addressing(&jid.to_string(), Instant::now())
}

/// How the group addresses its participants; `None` when it cannot be learned.
pub(crate) async fn group_addressing(sh: &Shared, client: &Client, jid: &Jid) -> Option<AddressingMode> {
    group_meta(sh, client, jid, &jid.to_string()).await.map(|m| m.addressing)
}

/// Network part for one group. Returns whether anything could be loaded.
async fn enrich_group(sh: &Shared, client: &Client, jid: &Jid, jid_str: &str, info: &mut ContactInfo) -> bool {
    let mut loaded = false;
    match group_meta_lookup(sh, client, jid, jid_str).await {
        MetaLookup::Found(meta) => {
            loaded = true;
            let me = own_jids(client);
            info.group = Some(group_info_from(&meta, &me, store_name_of(sh)));
        }
        MetaLookup::NotMember => {
            loaded = true;
            let mut group = info.group.take().unwrap_or_default();
            group.i_am_member = false;
            group.i_am_admin = false;
            info.group = Some(group);
        }
        MetaLookup::Unavailable => {}
    }

    let picture_id = sh
        .store
        .contact(jid_str)
        .ok()
        .flatten()
        .and_then(|c| c.avatar_id);
    match full_picture(sh, client, jid, jid_str, picture_id.as_deref()).await {
        Ok(path) => {
            loaded = true;
            if path.is_some() {
                info.picture_path = path;
            }
        }
        Err(e) => log::warn!("picture lookup for {} failed: {e:#}", redact(jid_str)),
    }
    loaded
}

/// Overlays the network data on `info`; `false` when nothing could be loaded.
async fn enrich(sh: &Shared, client: &Client, jid_str: &str, info: &mut ContactInfo) -> bool {
    let Ok(jid) = jid_str.parse::<Jid>() else {
        log::warn!("invalid jid in contact info request: {}", redact(jid_str));
        return false;
    };
    if jid.is_group() {
        if !client.is_connected() {
            return false;
        }
        enrich_group(sh, client, &jid, jid_str, info).await
    } else if jid.is_pn() || jid.is_lid() {
        if !client.is_connected() {
            return false;
        }
        enrich_user(sh, client, &jid, jid_str, info).await
    } else {
        // Status feed, newsletters...: nothing to look up.
        true
    }
}

/// Handles [`crate::api::Command::FetchContactInfo`].
pub async fn fetch(sh: Shared, client: Arc<Client>, jid: String) {
    match sh.info.claim(&jid, Instant::now()) {
        Claim::Cached(info) => {
            sh.emit(UiEvent::ContactInfo(*info));
            return;
        }
        Claim::Busy => {
            sh.emit(UiEvent::ContactInfo(load_local(&sh, &jid)));
            return;
        }
        Claim::Go => {}
    }
    // Each extra pass needs a new request made during the previous one.
    loop {
        run_claimed(&sh, &client, &jid).await;
        if !sh.info.take_rerun(&jid) {
            break;
        }
    }
}

/// One lookup whose claim the caller already owns.
async fn run_claimed(sh: &Shared, client: &Client, jid: &str) {
    let jid = jid.to_string();
    let guard = ClaimGuard::new(sh.info.clone(), jid.clone());
    let local = load_local(sh, &jid);
    sh.emit(UiEvent::ContactInfo(local.clone()));

    let mut info = local.clone();
    let loaded = enrich(sh, client, &jid, &mut info).await;
    if !loaded && !has_content(&local) {
        sh.emit(UiEvent::Error {
            message: "Could not load the contact info".to_string(),
        });
    }
    info.complete = true;
    if loaded {
        guard.finish(Instant::now(), info.clone());
    } else {
        // Offline or failed: do not pin the local-only answer for a minute.
        drop(guard);
    }
    sh.emit(UiEvent::ContactInfo(info));
}

#[cfg(test)]
mod tests {
    use super::*;

    const PN: &str = "491701234567@s.whatsapp.net";

    fn contact(full: Option<&str>, push: Option<&str>) -> ContactRow {
        ContactRow {
            jid: PN.into(),
            full_name: full.map(Into::into),
            push_name: push.map(Into::into),
            phone: None,
            avatar_path: None,
            avatar_id: None,
            avatar_checked_at: 0,
            about: Some("Busy".into()),
            about_set_at: Some(99),
        }
    }

    #[test]
    fn business_profile_is_mapped_without_empty_fields() {
        let mut p = biz::BusinessProfile::default();
        p.description = " Best bakery ".into();
        p.email = Some(String::new());
        p.address = Some("1 Main St".into());
        p.website = vec!["https://a.example".into(), " ".into()];
        let mapped = business_from(&p);
        assert_eq!(mapped.description.as_deref(), Some("Best bakery"));
        assert_eq!(mapped.email, None);
        assert_eq!(mapped.address.as_deref(), Some("1 Main St"));
        assert_eq!(mapped.category, None);
        assert_eq!(mapped.websites, vec!["https://a.example".to_string()]);
        assert!(mapped.hours.is_empty());
        assert_eq!(business_from(&biz::BusinessProfile::default()), BusinessProfile::default());
    }

    #[test]
    fn opening_hours_lines() {
        use biz::{BusinessHourMode as M, DayOfWeek as D};
        assert_eq!(hours_line(&D::Monday, &M::SpecificHours, Some(540), Some(1020)), "Mon: 09:00\u{2013}17:00");
        assert_eq!(hours_line(&D::Sunday, &M::Open24H, None, None), "Sun: Open 24 hours");
        assert_eq!(hours_line(&D::Friday, &M::AppointmentOnly, None, None), "Fri: By appointment only");
        assert_eq!(hours_line(&D::Saturday, &M::SpecificHours, None, None), "Sat: Open");
        assert_eq!(hours_line(&D::Tuesday, &M::SpecificHours, Some(0), Some(1440)), "Tue: 00:00\u{2013}00:00");
    }

    #[test]
    fn local_info_of_a_user_uses_the_stored_contact() {
        let c = contact(Some("Ann"), Some("annie"));
        let info = local_info(PN, Some(&c), None, Some(PathBuf::from("/a.jpg")));
        assert_eq!(info.name.as_deref(), Some("Ann"));
        assert_eq!(info.push_name.as_deref(), Some("annie"));
        assert_eq!(info.phone.as_deref(), Some("491701234567"));
        assert_eq!((info.about.as_deref(), info.about_set_at), (Some("Busy"), Some(99)));
        assert_eq!(info.picture_path, Some(PathBuf::from("/a.jpg")));
        assert!(!info.complete && !info.is_business && info.group.is_none());
    }

    #[test]
    fn local_info_without_a_contact_row_is_mostly_empty() {
        let info = local_info("100@lid", None, None, None);
        assert_eq!(info.phone, None, "a LID is not a phone number");
        assert!(!has_content(&info));
        assert_eq!(local_info(PN, None, None, None).phone.as_deref(), Some("491701234567"));

        let mut c = contact(Some(""), None);
        c.about = None;
        let info = local_info("100@lid", Some(&c), None, None);
        assert_eq!((info.name, info.about, info.about_set_at), (None, None, None));
    }

    #[test]
    fn local_info_of_a_group_carries_the_subject_and_no_participants() {
        let info = local_info("1-2@g.us", None, Some("Family"), None);
        let group = info.group.expect("group info");
        assert_eq!(group.subject, "Family");
        assert!(group.participants.is_empty());
        assert!(group.i_am_member, "membership is assumed until the metadata says otherwise");
        assert!(has_content(&ContactInfo {
            group: Some(group),
            ..ContactInfo::default()
        }));
        let unnamed = local_info("1-2@g.us", None, None, None);
        assert_eq!(unnamed.group.map(|g| g.subject), Some(String::new()));
        assert_eq!(unnamed.phone, None);
    }

    #[test]
    fn phone_of_jid_accepts_only_phone_number_jids() {
        assert_eq!(phone_of_jid(PN).as_deref(), Some("491701234567"));
        assert_eq!(phone_of_jid("491701234567:7@s.whatsapp.net").as_deref(), Some("491701234567"));
        assert_eq!(phone_of_jid("100@lid"), None);
        assert_eq!(phone_of_jid("1-2@g.us"), None);
        assert_eq!(phone_of_jid("@s.whatsapp.net"), None);
        assert_eq!(phone_of_jid("abc@s.whatsapp.net"), None);
    }

    fn raw(jid: &str, pn: Option<&str>, admin: bool, sup: bool) -> RawMember {
        RawMember {
            jid: jid.into(),
            pn: pn.map(Into::into),
            is_admin: admin,
            is_super_admin: sup,
        }
    }

    #[test]
    fn members_get_canonical_jids_names_phones_and_flags() {
        let members = [
            raw("100@lid", Some("4917011@s.whatsapp.net"), true, false),
            raw("4917022@s.whatsapp.net", None, false, false),
            raw("200@lid", None, false, false),
            raw("300@lid", Some("4917033@s.whatsapp.net"), true, true),
        ];
        let me = vec!["4917033@s.whatsapp.net".to_string(), "300@lid".to_string()];
        let names = |jid: &str| match jid {
            "4917011@s.whatsapp.net" => Some("Ann".to_string()),
            "200@lid" => Some("Lidonly".to_string()),
            _ => None,
        };
        let out = map_members(&members, &me, names);
        assert_eq!(out.len(), 4);

        assert_eq!(out[0].jid, "4917011@s.whatsapp.net");
        assert_eq!(out[0].name.as_deref(), Some("Ann"));
        assert_eq!(out[0].phone.as_deref(), Some("4917011"));
        assert!(out[0].is_admin && !out[0].is_super_admin && !out[0].is_me);

        assert_eq!(out[1].name.as_deref(), Some("+4917022"), "falls back to the phone");
        assert_eq!(out[1].phone.as_deref(), Some("4917022"));

        assert_eq!(out[2].jid, "200@lid");
        assert_eq!(out[2].name.as_deref(), Some("Lidonly"), "name stored under the LID");
        assert_eq!(out[2].phone, None);

        assert!(out[3].is_me && out[3].is_admin && out[3].is_super_admin);
    }

    #[test]
    fn my_membership_matches_phone_and_lid_forms() {
        let members = [
            raw("100@lid", Some("4917011@s.whatsapp.net"), false, false),
            raw("300@lid", Some("4917033@s.whatsapp.net"), true, false),
            raw("4917044@s.whatsapp.net", None, false, true),
        ];
        let by_pn = vec!["4917033@s.whatsapp.net".to_string()];
        assert_eq!(my_membership(&members, &by_pn), (true, true));
        let by_lid = vec!["100@lid".to_string()];
        assert_eq!(my_membership(&members, &by_lid), (false, true));
        let both = vec!["4917044@s.whatsapp.net".to_string(), "999@lid".to_string()];
        assert_eq!(my_membership(&members, &both), (true, true), "super admin counts as admin");
        let stranger = vec!["4917055@s.whatsapp.net".to_string()];
        assert_eq!(my_membership(&members, &stranger), (false, false));
        assert_eq!(my_membership(&members, &[]), (false, false));
    }

    #[test]
    fn group_info_carries_settings_and_my_role() {
        let meta = GroupMeta {
            subject: "G".into(),
            description: None,
            created_at: None,
            creator: None,
            members: vec![raw("4917033@s.whatsapp.net", None, true, false)],
            addressing: AddressingMode::Pn,
            announce_only: true,
            locked: true,
            approval_required: true,
        };
        let me = vec!["4917033@s.whatsapp.net".to_string()];
        let info = group_info_from(&meta, &me, |_| None);
        assert!(info.announce_only && info.locked && info.approval_required);
        assert!(info.i_am_admin && info.i_am_member);
        let left = group_info_from(&meta, &["1@lid".to_string()], |_| None);
        assert!(!left.i_am_admin && !left.i_am_member);
    }

    #[test]
    fn not_member_codes() {
        assert!(is_not_member_code(401) && is_not_member_code(403) && is_not_member_code(404));
        assert!(!is_not_member_code(500) && !is_not_member_code(409));
    }

    #[test]
    fn best_name_prefers_the_address_book() {
        assert_eq!(best_name(&contact(Some("Ann"), Some("x"))).as_deref(), Some("Ann"));
        assert_eq!(best_name(&contact(Some(""), Some("x"))).as_deref(), Some("x"));
        assert_eq!(best_name(&contact(None, None)), None);
    }

    #[test]
    fn a_jid_is_looked_up_once_per_minute() {
        let cache = InfoCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.claim("a", t0), Claim::Go);
        assert_eq!(cache.claim("a", t0), Claim::Busy, "already running");
        assert_eq!(cache.claim("b", t0), Claim::Go, "other jids are independent");

        let info = ContactInfo {
            jid: "a".into(),
            complete: true,
            ..ContactInfo::default()
        };
        cache.finish("a", t0, info.clone());
        let just_inside = t0 + INFO_TTL - Duration::from_secs(1);
        assert_eq!(cache.claim("a", just_inside), Claim::Cached(Box::new(info)));
        assert_eq!(cache.claim("a", t0 + INFO_TTL), Claim::Go, "window elapsed");
    }

    #[test]
    fn a_lookup_invalidated_while_running_is_not_cached() {
        let cache = InfoCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.claim("a", t0), Claim::Go);
        cache.invalidate("a");
        cache.finish("a", t0, ContactInfo::default());
        assert_eq!(cache.claim("a", t0), Claim::Go, "stale result was dropped and the claim released");

        let info = ContactInfo { jid: "a".into(), complete: true, ..ContactInfo::default() };
        cache.finish("a", t0, info.clone());
        assert_eq!(cache.claim("a", t0), Claim::Cached(Box::new(info)), "a lookup started after the bump caches");
    }

    #[test]
    fn requests_during_a_lookup_coalesce_into_one_rerun() {
        let cache = InfoCache::default();
        let t0 = Instant::now();
        assert!(!cache.take_rerun("a"), "nothing asked, nothing to rerun");
        assert_eq!(cache.claim("a", t0), Claim::Go);
        assert_eq!(cache.claim("a", t0), Claim::Busy);
        assert_eq!(cache.claim("a", t0), Claim::Busy);
        cache.invalidate("a");
        cache.finish("a", t0, ContactInfo::default());
        assert!(cache.take_rerun("a"), "a rerun was requested");
        let info = ContactInfo { jid: "a".into(), complete: true, ..ContactInfo::default() };
        cache.finish("a", t0, info.clone());
        assert!(!cache.take_rerun("a"), "several requests caused only one rerun");
        assert_eq!(cache.claim("a", t0), Claim::Cached(Box::new(info)));
    }

    #[test]
    fn a_rerun_skips_the_cached_result() {
        let cache = InfoCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.claim("a", t0), Claim::Go);
        assert_eq!(cache.claim("a", t0), Claim::Busy);
        cache.finish("a", t0, ContactInfo { jid: "old".into(), ..ContactInfo::default() });
        assert!(cache.take_rerun("a"));
        assert_eq!(cache.claim("a", t0), Claim::Busy, "old result no longer served; rerun in flight");
    }

    #[test]
    fn a_new_claim_forgets_a_leftover_request() {
        let cache = Arc::new(InfoCache::default());
        let t0 = Instant::now();
        assert_eq!(cache.claim("a", t0), Claim::Go);
        assert_eq!(cache.claim("a", t0), Claim::Busy);
        drop(ClaimGuard::new(cache.clone(), "a".into()));
        assert_eq!(cache.claim("a", t0), Claim::Go);
        cache.finish("a", t0, ContactInfo::default());
        assert!(!cache.take_rerun("a"));
    }

    #[test]
    fn dropping_the_guard_releases_the_claim_without_caching() {
        let cache = Arc::new(InfoCache::default());
        let t0 = Instant::now();
        assert_eq!(cache.claim("a", t0), Claim::Go);
        let guard = ClaimGuard::new(cache.clone(), "a".into());
        assert_eq!(cache.claim("a", t0), Claim::Busy);
        drop(guard);
        assert_eq!(cache.claim("a", t0), Claim::Go, "released, nothing cached");

        let guard = ClaimGuard::new(cache.clone(), "a".into());
        let info = ContactInfo { jid: "a".into(), complete: true, ..ContactInfo::default() };
        guard.finish(t0, info.clone());
        assert_eq!(cache.claim("a", t0), Claim::Cached(Box::new(info)), "finish caches");
    }

    #[test]
    fn a_failed_lookup_is_retried_and_invalidate_forgets_results() {
        let cache = Arc::new(InfoCache::default());
        let t0 = Instant::now();
        assert_eq!(cache.claim("a", t0), Claim::Go);
        drop(ClaimGuard::new(cache.clone(), "a".into()));
        assert_eq!(cache.claim("a", t0), Claim::Go, "failure was not cached");

        cache.finish("a", t0, ContactInfo::default());
        cache.put_group_meta("a", t0, GroupMeta {
            subject: "G".into(), description: None, created_at: None, creator: None,
            members: vec![], addressing: AddressingMode::Pn,
            announce_only: false, locked: false, approval_required: false,
        });
        cache.invalidate("a");
        assert_eq!(cache.group_meta("a", t0), None);
        assert_eq!(cache.claim("a", t0), Claim::Go);
    }

    #[test]
    fn group_metadata_is_reused_for_five_minutes() {
        let cache = InfoCache::default();
        let t0 = Instant::now();
        let meta = GroupMeta {
            subject: "G".into(),
            description: None,
            created_at: Some(5),
            creator: None,
            members: vec![],
            addressing: AddressingMode::Lid,
            announce_only: false,
            locked: false,
            approval_required: false,
        };
        assert_eq!(cache.group_meta("g", t0), None);
        cache.put_group_meta("g", t0, meta.clone());
        assert_eq!(cache.group_meta("g", t0 + GROUP_META_TTL - Duration::from_secs(1)), Some(meta));
        assert_eq!(cache.group_meta("g", t0 + GROUP_META_TTL), None);

        cache.put_group_meta("g", t0, GroupMeta { subject: "H".into(), description: None, created_at: None, creator: None, members: vec![], addressing: AddressingMode::Pn, announce_only: false, locked: false, approval_required: false });
        cache.clear();
        assert_eq!(cache.group_meta("g", t0), None);
    }
}
