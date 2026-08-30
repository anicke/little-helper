//! The tracker list: the eleven sites Trader's Little Helper shipped, what has become of
//! them, and the user's own list on top.
//!
//! TLH's `tracker.lst` is dated 2018-08-03 inside a build released 2020-10-15, and it
//! carries no date anywhere a user can see. That is how it went on recommending a host with
//! no DNS record and a forum root that was never an announce URL, for years, without anyone
//! noticing. So every entry here carries the date we last checked it and *what we saw*, and
//! `lh torrent trackers` prints both. A list with no date asks to be trusted; a list with a
//! date asks to be checked.
//!
//! The checking is why [`Health`] is an enum and not a boolean. Of TLH's eleven, on
//! 2026-08-30, two answer as trackers, two answer as trackers but only to a personal
//! announce URL, four cannot work at that URL at all, and three did not answer from here —
//! which is not the same thing as gone, and is not written down as if it were.

use crate::config;
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The user's own list, in the `Name|URL` format TLH used, so a curated list can be
/// brought across unchanged.
pub const TRACKER_LIST_FILE: &str = "trackers.lst";

/// `id|passkey`, one per line — the same shape as the tracker list, for the same reason:
/// one format in this directory rather than two.
pub const PASSKEY_FILE: &str = "passkeys.lst";

/// What an announce URL carries where the site issues a per-user key.
pub const PASSKEY_PLACEHOLDER: &str = "{passkey}";

/// The date the bundled entries below were last checked. One constant because they were all
/// checked in one sweep; if that stops being true, this becomes a per-entry field.
const CHECKED: &str = "2026-08-30";

/// What we know about an entry, and therefore whether we may write its URL into a torrent.
///
/// An announce URL that cannot work is worse than no tracker at all, because the user finds
/// out only when nobody ever connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// It answered as a tracker. Use it.
    Announces,
    /// It answered as a tracker, but only a personal announce URL will authorize: the
    /// generic one in the list authorizes nobody. We refuse the id and say where to get
    /// the real URL, because writing this one produces a torrent that silently never
    /// announces.
    PersonalUrl,
    /// This URL cannot work — it is not a tracker any more, or never was, or does not
    /// resolve. The site may well still exist somewhere else; the URL is what is broken.
    Broken,
    /// Nothing answered from where we checked. **Not** evidence that it is gone: a
    /// firewall between us and them looks exactly like this. Usable, with a warning.
    Unreachable,
    /// A user's own entry. We have never checked it and we use it exactly as given.
    Unchecked,
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Self::Announces => "announces",
            Self::PersonalUrl => "personal URL needed",
            Self::Broken => "broken",
            Self::Unreachable => "unreachable",
            Self::Unchecked => "unchecked",
        }
    }

    /// Whether we are willing to write this entry's URL into a torrent.
    pub fn usable(self) -> bool {
        !matches!(self, Self::PersonalUrl | Self::Broken)
    }
}

/// Where an entry came from, so `lh torrent trackers` can show which ones are ours and
/// which the user brought — including which of ours a user list replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Bundled,
    /// From the user's `trackers.lst`.
    User,
    /// A bundled entry the user's list replaced by id.
    Overridden,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    /// What `--tracker` accepts: "etree", "dime".
    pub id: String,
    pub name: String,
    /// May contain [`PASSKEY_PLACEHOLDER`].
    pub announce: String,
    /// Sets `private: 1` inside the info dictionary, which changes the infohash.
    pub private: bool,
    /// Some private sites require an `info.source` to make the infohash theirs.
    pub source: Option<String>,
    pub health: Health,
    /// ISO date we last checked. `None` for an entry we have never checked.
    pub checked: Option<String>,
    /// What we saw when we checked — the tracker's own words wherever it spoke.
    pub evidence: Option<String>,
    pub origin: Origin,
}

impl Tracker {
    pub fn needs_passkey(&self) -> bool {
        self.announce.contains(PASSKEY_PLACEHOLDER)
    }

    /// The announce URL with the passkey filled in, or an error naming the file the key
    /// should have come from. A torrent with an unresolved `{passkey}` is never written.
    pub fn announce_url(&self, keys: &Passkeys) -> Result<String> {
        if !self.needs_passkey() {
            return Ok(self.announce.clone());
        }
        match keys.get(&self.id) {
            Some(key) => Ok(self.announce.replace(PASSKEY_PLACEHOLDER, key)),
            None => Err(Error::UnusableTracker {
                id: self.id.clone(),
                detail: format!(
                    "{} issues a personal announce key and none is configured; put \
                     `{}|<your key>` in {}",
                    self.name,
                    self.id,
                    config::config_path(PASSKEY_FILE)
                        .unwrap_or_else(|| PathBuf::from(PASSKEY_FILE))
                        .display()
                ),
            }),
        }
    }
}

/// TLH's eleven, dated and with what we saw when we looked.
///
/// **None of them is marked private, and none carries a `{passkey}`.** Both are mechanisms
/// this module implements and the user's own list can use; neither is asserted here,
/// because we have no evidence for either. `private` lives inside the info dictionary, so
/// guessing it wrong silently changes the infohash of every torrent made for that site —
/// and TLH is no help, its changelog shows `Private torrent` as a manual checkbox and no
/// passkey support at all. Two entries below plainly *do* need a personal URL; what we do
/// not know is the shape of it, so they say so rather than shipping a guessed template.
const BUNDLED: &[(&str, &str, &str, Health, &str)] = &[
    (
        "crosstown",
        "Crosstown Torrents",
        "http://crosstowntorrents.org:5555/announce",
        Health::Unreachable,
        "the host resolves but nothing answered on port 5555",
    ),
    (
        "dime",
        "DIME",
        "http://bt.dimeadozen.org/announce.php",
        Health::PersonalUrl,
        "answered: \"not authorized; download a new copy of the .torrent file from the tracker\"",
    ),
    (
        "etree",
        "etree.org",
        "http://tracker.etree.org:6969/announce",
        Health::PersonalUrl,
        "answered: \"Missing Key.\"",
    ),
    (
        "genesis",
        "Genesis-Movement Torrent",
        "http://torrent.genesis-movement.org/announce.php",
        Health::Announces,
        "answered as a tracker: \"Invalid info hash value.\"",
    ),
    (
        "jamtothis",
        "JamToThis",
        "http://www.jamtothis.com:2710/announce",
        Health::Unreachable,
        "the host resolves but nothing answered on port 2710",
    ),
    (
        "losslesslegs",
        "Lossless Legs",
        "http://www.shnflac.net/announce.php",
        Health::Broken,
        "port 80 refuses connections; the site answers on 443 but not as a tracker",
    ),
    (
        "mindwarp",
        "Mind-Warp PaVilion",
        "http://www.mindwarppavilion.org/ezt/announce.php",
        Health::Broken,
        "redirects to the site root and returns a web page, not a tracker response",
    ),
    (
        "tradersden",
        "The Traders' Den",
        "http://www.thetradersden.org/forums/tracker/announce.php",
        Health::Announces,
        "answered as a tracker: \"Invalid info_hash (0 - )\"",
    ),
    (
        "yeeshkul",
        "YEESHKUL!",
        "http://www.yeeshkul.com:2710/announce",
        Health::Unreachable,
        "the host resolves but nothing answered on port 2710",
    ),
    (
        "zappateers",
        "Zappateers",
        "http://www.zappateers.com/bb/",
        Health::Broken,
        "a static page reading \"Zappateers is currently off-line for a major overhaul\", \
         dated 2020-08-18; TLH shipped a forum root here, never an announce URL",
    ),
    (
        "zomb",
        "ZOMB Torrents",
        "http://t1.the-zomb.com/announce.php",
        Health::Broken,
        "no DNS A record",
    ),
];

/// The bundled list, plus whatever the user's own list adds to or replaces in it.
#[derive(Debug, Clone, Default)]
pub struct TrackerList {
    entries: Vec<Tracker>,
    /// The user list we read, if there was one. Shown so a user can see which file is in
    /// play — and, when there is none, where to put one.
    pub user_list: Option<PathBuf>,
}

impl TrackerList {
    /// Just the entries we ship.
    pub fn bundled() -> Self {
        let entries = BUNDLED
            .iter()
            .map(|(id, name, announce, health, evidence)| Tracker {
                id: (*id).to_string(),
                name: (*name).to_string(),
                announce: (*announce).to_string(),
                private: false,
                source: None,
                health: *health,
                checked: Some(CHECKED.to_string()),
                evidence: Some((*evidence).to_string()),
                origin: Origin::Bundled,
            })
            .collect();
        Self {
            entries,
            user_list: None,
        }
    }

    /// The bundled list with the user's `trackers.lst` merged over it, if there is one.
    pub fn load() -> Result<Self> {
        let mut list = Self::bundled();
        let Some(path) = config::config_path(TRACKER_LIST_FILE) else {
            return Ok(list);
        };
        match std::fs::read(&path) {
            Ok(bytes) => {
                list.merge(&bytes, &path)?;
                list.user_list = Some(path);
            }
            // No list is the normal case, not a failure. Remember where it would go so the
            // user can be told.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => list.user_list = Some(path),
            Err(e) => return Err(Error::io(&path, e)),
        }
        Ok(list)
    }

    /// Merge a `Name|URL` list over this one. An entry whose id already exists replaces it;
    /// the rest are appended, in file order.
    pub fn merge(&mut self, bytes: &[u8], path: &Path) -> Result<()> {
        for entry in parse_list(bytes, path)? {
            match self.entries.iter().position(|t| t.id == entry.id) {
                Some(i) => {
                    // Keep the bundled entry visible, marked as replaced, so `trackers`
                    // shows that an override happened rather than the original vanishing.
                    self.entries[i].origin = Origin::Overridden;
                    self.entries.insert(i, entry);
                }
                None => self.entries.push(entry),
            }
        }
        Ok(())
    }

    /// Entries that can still be chosen — everything a user list did not replace.
    pub fn iter(&self) -> impl Iterator<Item = &Tracker> {
        self.entries
            .iter()
            .filter(|t| t.origin != Origin::Overridden)
    }

    /// Every entry, replaced ones included.
    pub fn all(&self) -> impl Iterator<Item = &Tracker> {
        self.entries.iter()
    }

    pub fn get(&self, id: &str) -> Option<&Tracker> {
        self.iter().find(|t| t.id == id)
    }

    /// The ids we do have, for the error we print when asked for one we do not (Principle 5).
    pub fn ids(&self) -> String {
        let ids: Vec<&str> = self.iter().map(|t| t.id.as_str()).collect();
        ids.join(", ")
    }
}

/// Parse TLH's `Display Name|announce URL` format.
///
/// TLH's own files are exactly two fields, CRLF-terminated, with no comments and no header,
/// and those are read unchanged. Anything after the URL is ours: `private`, `source=TAG`,
/// `id=SLUG`. `#` starts a comment, which TLH's files never contain, so nothing of theirs
/// is misread.
pub fn parse_list(bytes: &[u8], path: &Path) -> Result<Vec<Tracker>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::malformed(path, format!("not valid UTF-8: {e}")))?;

    let mut out: Vec<Tracker> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |detail: String| Error::malformed(path, format!("line {}: {detail}", n + 1));

        let mut fields = line.split('|');
        let name = fields.next().unwrap_or_default().trim();
        let announce = fields
            .next()
            .ok_or_else(|| {
                at(format!(
                    "{line:?} has no `|`; the format is `Display Name|announce URL`"
                ))
            })?
            .trim();
        if name.is_empty() {
            return Err(at("the entry has no name".into()));
        }
        if announce.is_empty() {
            return Err(at(format!("{name:?} has no announce URL")));
        }

        let mut tracker = Tracker {
            id: slug(name),
            name: name.to_string(),
            announce: announce.to_string(),
            private: false,
            source: None,
            health: Health::Unchecked,
            checked: None,
            evidence: None,
            origin: Origin::User,
        };
        for field in fields {
            let field = field.trim();
            match field.split_once('=') {
                Some(("id", v)) => tracker.id = v.trim().to_string(),
                Some(("source", v)) => tracker.source = Some(v.trim().to_string()),
                Some((key, _)) => return Err(at(format!("unknown field {key:?}"))),
                None if field == "private" => tracker.private = true,
                None if field.is_empty() => {}
                None => return Err(at(format!("unknown field {field:?}"))),
            }
        }
        if tracker.id.is_empty() {
            return Err(at(format!(
                "{name:?} gives no id and none can be made from it"
            )));
        }
        if let Some(other) = out.iter().find(|t| t.id == tracker.id) {
            return Err(at(format!(
                "id {:?} is already used by {:?}; give one of them an `id=` field",
                tracker.id, other.name
            )));
        }
        out.push(tracker);
    }
    Ok(out)
}

/// An id from a display name: lowercase, letters and digits only. "Lossless Legs" becomes
/// "losslesslegs", which is what the bundled table calls it.
fn slug(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Per-tracker announce keys, from `passkeys.lst`.
#[derive(Debug, Clone, Default)]
pub struct Passkeys {
    keys: BTreeMap<String, String>,
    pub path: Option<PathBuf>,
}

impl Passkeys {
    pub fn load() -> Result<Self> {
        let Some(path) = config::config_path(PASSKEY_FILE) else {
            return Ok(Self::default());
        };
        let mut keys = match std::fs::read(&path) {
            Ok(bytes) => Self::parse(&bytes, &path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(Error::io(&path, e)),
        };
        keys.path = Some(path);
        Ok(keys)
    }

    pub fn parse(bytes: &[u8], path: &Path) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| Error::malformed(path, format!("not valid UTF-8: {e}")))?;
        let mut keys = BTreeMap::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (id, key) = line.split_once('|').ok_or_else(|| {
                Error::malformed(
                    path,
                    format!("line {}: {line:?} is not `tracker id|passkey`", n + 1),
                )
            })?;
            keys.insert(id.trim().to_string(), key.trim().to_string());
        }
        Ok(Self { keys, path: None })
    }

    pub fn get(&self, id: &str) -> Option<&str> {
        self.keys.get(id).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// One tracker as it will go into the torrent.
#[derive(Debug, Clone)]
pub struct Chosen {
    /// The list entry it came from. `None` when the user gave a bare URL, which is used
    /// verbatim — we never substitute or "correct" a URL somebody typed.
    pub tracker: Option<Tracker>,
    /// The announce URL, passkey filled in.
    pub announce: String,
}

impl Chosen {
    pub fn name(&self) -> &str {
        match &self.tracker {
            Some(t) => &t.name,
            None => "(given as a URL)",
        }
    }
}

/// Trackers, and what choosing them implies for the torrent itself.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// One tier per `--tracker`, in the order given. Clients pick at random within a tier
    /// and fall through between tiers, so unrelated sites must not share one.
    pub tiers: Vec<Vec<String>>,
    /// Set by any chosen tracker whose entry is private. This is inside the info
    /// dictionary, so it is part of the infohash — which is why the caller must say it did.
    pub private: bool,
    pub source: Option<String>,
    pub chosen: Vec<Chosen>,
    /// Things the user should know that are not reasons to stop.
    pub warnings: Vec<String>,
}

/// Turn `--tracker` arguments — ids from the list, or URLs — into tiers.
///
/// Refuses rather than writes an announce URL we know cannot work: an unknown id, an entry
/// whose URL is broken, an entry that needs a personal URL we do not have, or an unresolved
/// `{passkey}`. Each refusal names the escape hatch, which is always the same one: pass the
/// URL you know is right and we will use it verbatim.
pub fn resolve(specs: &[String], list: &TrackerList, keys: &Passkeys) -> Result<Resolved> {
    let mut out = Resolved::default();
    let mut private_sites: Vec<String> = Vec::new();

    for spec in specs {
        let chosen = if spec.contains("://") {
            Chosen {
                tracker: None,
                announce: spec.clone(),
            }
        } else {
            let tracker = list.get(spec).ok_or_else(|| Error::UnusableTracker {
                id: spec.clone(),
                detail: format!(
                    "no tracker by that name. Known ids: {}. An announce URL is taken \
                     verbatim, but it needs a scheme (http://…)",
                    list.ids()
                ),
            })?;
            refuse_unusable(tracker)?;
            if tracker.health == Health::Unreachable {
                out.warnings.push(format!(
                    "{} did not answer when we checked on {} ({}). That is not proof it is \
                     gone — it may just be unreachable from where we looked.",
                    tracker.name,
                    tracker.checked.as_deref().unwrap_or("an unknown date"),
                    tracker
                        .evidence
                        .as_deref()
                        .unwrap_or("no detail was recorded"),
                ));
            }
            let announce = tracker.announce_url(keys)?;
            if tracker.private {
                private_sites.push(tracker.name.clone());
                out.private = true;
            }
            if let Some(source) = &tracker.source {
                match &out.source {
                    Some(existing) if existing != source => {
                        return Err(Error::UnusableTracker {
                            id: tracker.id.clone(),
                            detail: format!(
                                "wants info.source {source:?} but another tracker wants \
                                 {existing:?}; source is inside the infohash, so one \
                                 torrent cannot satisfy both"
                            ),
                        });
                    }
                    _ => out.source = Some(source.clone()),
                }
            }
            Chosen {
                tracker: Some(tracker.clone()),
                announce,
            }
        };

        if out.chosen.iter().any(|c| c.announce == chosen.announce) {
            out.warnings.push(format!(
                "{} was named twice; using it once",
                chosen.announce
            ));
            continue;
        }
        out.tiers.push(vec![chosen.announce.clone()]);
        out.chosen.push(chosen);
    }

    // A warning, not a block: cross-posting a private torrent breaks most sites' rules and
    // gets accounts banned — but it is the user's account, and they may know something we
    // do not.
    if private_sites.len() > 1 {
        out.warnings.push(format!(
            "you named {} private sites ({}). Cross-posting one torrent to several breaks \
             most sites' rules.",
            private_sites.len(),
            private_sites.join(", ")
        ));
    }
    Ok(out)
}

fn refuse_unusable(tracker: &Tracker) -> Result<()> {
    if tracker.health.usable() {
        return Ok(());
    }
    let checked = tracker.checked.as_deref().unwrap_or("an unknown date");
    let saw = tracker
        .evidence
        .as_deref()
        .unwrap_or("no detail was recorded");
    let detail = match tracker.health {
        Health::PersonalUrl => format!(
            "{} issues a personal announce URL; the one in the list authorizes nobody. \
             Checked {checked}: {saw}. Get yours from the site and pass it with \
             --tracker <URL>.",
            tracker.name
        ),
        _ => format!(
            "{} cannot work. Checked {checked}: {saw}. If you know a URL that does, pass \
             it with --tracker <URL>.",
            tracker.announce
        ),
    };
    Err(Error::UnusableTracker {
        id: tracker.id.clone(),
        detail,
    })
}
