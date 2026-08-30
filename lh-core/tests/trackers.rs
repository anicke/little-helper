//! The tracker list.
//!
//! What is worth testing here is not "does it parse" but "does it refuse". The whole reason
//! this list carries dates and evidence is that Trader's Little Helper shipped one that did
//! not, and went on offering a host with no DNS record and a forum root for years. So the
//! assertions are mostly about the entries we know cannot work never reaching a torrent.

use lh_core::torrent::trackers::{
    Health, Origin, PASSKEY_PLACEHOLDER, Passkeys, Tracker, TrackerList, parse_list, resolve,
};
use std::path::Path;

fn specs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn user_list(text: &str) -> TrackerList {
    let mut list = TrackerList::bundled();
    list.merge(text.as_bytes(), Path::new("trackers.lst"))
        .unwrap();
    list
}

/// Every bundled entry says when it was checked and what was seen. An entry with a status
/// and no date asks to be trusted; that is the failure this list exists to avoid.
#[test]
fn every_bundled_entry_is_dated_and_carries_its_evidence() {
    let list = TrackerList::bundled();
    assert_eq!(list.iter().count(), 11, "TLH shipped eleven");
    for t in list.iter() {
        assert!(t.checked.is_some(), "{} has no check date", t.id);
        assert!(t.evidence.is_some(), "{} says nothing about why", t.id);
        assert_eq!(t.origin, Origin::Bundled);
        // Neither flag is asserted for any site we ship: both change the infohash, and we
        // have no evidence for either. See the comment on BUNDLED.
        assert!(!t.private, "{} claims private without evidence", t.id);
        assert!(!t.needs_passkey(), "{} guesses a passkey URL shape", t.id);
    }
}

/// The four entries we checked and found unusable, and the two that need a personal URL,
/// never become an announce URL in a torrent. A URL that cannot work is worse than no
/// tracker at all, because the user finds out only when nobody ever connects.
#[test]
fn an_entry_we_know_cannot_work_is_refused_by_id() {
    let list = TrackerList::bundled();
    let keys = Passkeys::default();

    for id in ["zomb", "zappateers", "mindwarp", "losslesslegs"] {
        let err = resolve(&specs(&[id]), &list, &keys).expect_err("must refuse");
        let message = err.to_string();
        assert!(message.contains("cannot work"), "{id}: {message}");
        // Always with the escape hatch, because we might be the ones who are wrong.
        assert!(message.contains("--tracker <URL>"), "{id}: {message}");
    }

    for id in ["dime", "etree"] {
        let err = resolve(&specs(&[id]), &list, &keys).expect_err("must refuse");
        let message = err.to_string();
        assert!(message.contains("personal announce URL"), "{id}: {message}");
    }
}

/// Unreachable is not gone. A firewall between us and a tracker looks exactly like a dead
/// tracker, so it warns and proceeds rather than refusing on our own network's evidence.
#[test]
fn an_unreachable_entry_warns_but_is_still_used() {
    let list = TrackerList::bundled();
    let out = resolve(&specs(&["crosstown"]), &list, &Passkeys::default()).unwrap();
    assert_eq!(
        out.tiers,
        vec![vec![
            "http://crosstowntorrents.org:5555/announce".to_string()
        ]]
    );
    assert_eq!(out.warnings.len(), 1);
    assert!(out.warnings[0].contains("not proof it is gone"), "{out:?}");
}

/// A URL is used verbatim. We never silently substitute or "correct" one somebody typed —
/// including one for a site whose bundled entry we have marked broken.
#[test]
fn a_url_is_used_exactly_as_given() {
    let list = TrackerList::bundled();
    let url = "http://t1.the-zomb.com/announce.php";
    let out = resolve(&specs(&[url]), &list, &Passkeys::default()).unwrap();
    assert_eq!(out.tiers, vec![vec![url.to_string()]]);
    assert!(out.chosen[0].tracker.is_none());
    assert!(out.warnings.is_empty());
}

/// Principle 5: an unknown id names the ones we do have.
#[test]
fn an_unknown_id_lists_the_ids_we_have() {
    let err = resolve(
        &specs(&["etreeorg"]),
        &TrackerList::bundled(),
        &Passkeys::default(),
    )
    .expect_err("must refuse");
    let message = err.to_string();
    assert!(message.contains("etree"), "{message}");
    assert!(message.contains("tradersden"), "{message}");
    assert!(message.contains("needs a scheme"), "{message}");
}

/// One tier per `--tracker`, in the order given. Clients pick at random within a tier and
/// fall through between tiers, so unrelated sites sharing one is a coin flip over who
/// hears about the seed.
#[test]
fn each_tracker_becomes_its_own_tier_in_order() {
    let out = resolve(
        &specs(&["tradersden", "genesis", "http://custom.example/announce"]),
        &TrackerList::bundled(),
        &Passkeys::default(),
    )
    .unwrap();
    assert_eq!(
        out.tiers,
        vec![
            vec!["http://www.thetradersden.org/forums/tracker/announce.php".to_string()],
            vec!["http://torrent.genesis-movement.org/announce.php".to_string()],
            vec!["http://custom.example/announce".to_string()],
        ]
    );
}

/// TLH's own file, byte for byte, is the compatibility requirement: two fields, CRLF, no
/// comments, no header. Ours reads it unchanged.
#[test]
fn tlhs_own_format_is_read_unchanged() {
    let tlh = "Crosstown Torrents|http://crosstowntorrents.org:5555/announce\r\n\
               DIME|http://bt.dimeadozen.org/announce.php\r\n\
               Some New Site|http://new.example/announce\r\n";
    let entries = parse_list(tlh.as_bytes(), Path::new("tracker.lst")).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].name, "Crosstown Torrents");
    assert_eq!(entries[0].id, "crosstowntorrents");
    assert_eq!(entries[2].announce, "http://new.example/announce");
    // Nothing in a user's list has been checked by us, and it does not pretend otherwise.
    assert!(entries.iter().all(|t| t.health == Health::Unchecked));
    assert!(entries.iter().all(|t| t.checked.is_none()));
}

/// A user list extends and overrides the bundled one, and the entry it replaced stays
/// visible rather than vanishing — otherwise a typo'd id looks like a missing tracker.
#[test]
fn a_user_list_overrides_by_id_and_says_so() {
    let list = user_list(
        "etree.org|http://tracker.etree.org:6969/mykey/announce|id=etree\n\
         The Pit|https://pit.example/announce\n",
    );
    let etree = list.get("etree").unwrap();
    assert_eq!(etree.origin, Origin::User);
    assert_eq!(
        etree.announce,
        "http://tracker.etree.org:6969/mykey/announce"
    );
    assert!(list.get("thepit").is_some());

    // The bundled entry is still in `all()`, marked as replaced, and out of `iter()`.
    assert_eq!(list.iter().filter(|t| t.id == "etree").count(), 1);
    assert_eq!(list.all().filter(|t| t.id == "etree").count(), 2);
    assert!(
        list.all()
            .any(|t| t.id == "etree" && t.origin == Origin::Overridden)
    );

    // And the override is usable, where the bundled entry it replaced was not.
    let out = resolve(&specs(&["etree"]), &list, &Passkeys::default()).unwrap();
    assert_eq!(
        out.tiers[0][0],
        "http://tracker.etree.org:6969/mykey/announce"
    );
}

/// The fields past the URL are ours; TLH's files never have them.
#[test]
fn a_user_entry_can_set_private_and_source() {
    let list = user_list("The Pit|https://pit.example/announce|private|source=PIT\n");
    let out = resolve(&specs(&["thepit"]), &list, &Passkeys::default()).unwrap();
    assert!(out.private, "a private tracker sets the flag");
    assert_eq!(out.source.as_deref(), Some("PIT"));
}

/// Two sites wanting different `source` tags cannot be one torrent: source is inside the
/// info dictionary, so it is part of the infohash. Say so instead of picking one.
#[test]
fn two_trackers_wanting_different_sources_is_an_error() {
    let list = user_list(
        "A|https://a.example/announce|source=AAA\n\
         B|https://b.example/announce|source=BBB\n",
    );
    let err = resolve(&specs(&["a", "b"]), &list, &Passkeys::default()).expect_err("must refuse");
    assert!(err.to_string().contains("cannot satisfy both"), "{err}");
}

/// Cross-posting a private torrent breaks most sites' rules — but it is the user's account,
/// and they may know something we do not. Warn, do not block.
#[test]
fn several_private_trackers_warn_rather_than_stop() {
    let list = user_list(
        "A|https://a.example/announce|private\n\
         B|https://b.example/announce|private\n",
    );
    let out = resolve(&specs(&["a", "b"]), &list, &Passkeys::default()).unwrap();
    assert_eq!(out.tiers.len(), 2);
    assert!(
        out.warnings.iter().any(|w| w.contains("Cross-posting")),
        "{:?}",
        out.warnings
    );
}

/// A torrent with an unresolved `{passkey}` is never written: nobody would ever connect,
/// and the user would only find that out much later.
#[test]
fn an_unfilled_passkey_is_refused_and_a_filled_one_is_substituted() {
    let list = user_list("The Pit|https://pit.example/announce/{passkey}|private\n");
    assert!(list.get("thepit").unwrap().needs_passkey());

    let err = resolve(&specs(&["thepit"]), &list, &Passkeys::default()).expect_err("must refuse");
    let message = err.to_string();
    assert!(message.contains("personal announce key"), "{message}");
    assert!(message.contains("thepit|<your key>"), "{message}");

    let keys = Passkeys::parse(b"thepit|s3cr3t\n", Path::new("passkeys.lst")).unwrap();
    let out = resolve(&specs(&["thepit"]), &list, &keys).unwrap();
    assert_eq!(out.tiers[0][0], "https://pit.example/announce/s3cr3t");
    assert!(!out.tiers[0][0].contains(PASSKEY_PLACEHOLDER));
}

/// A malformed line is named by number, not skipped. A silently dropped tracker is a
/// torrent that quietly goes nowhere.
#[test]
fn a_malformed_line_names_its_line_number() {
    for (text, expect) in [
        ("Good|http://a.example/announce\nno pipe here\n", "line 2"),
        (
            "Good|http://a.example/announce\n|http://b.example\n",
            "line 2",
        ),
        ("Good|\n", "line 1"),
        ("Good|http://a.example|nonsense\n", "line 1"),
        (
            "A|http://a.example/announce\nA|http://b.example/announce\n",
            "already used",
        ),
    ] {
        let err = parse_list(text.as_bytes(), Path::new("trackers.lst")).expect_err("must refuse");
        assert!(err.to_string().contains(expect), "{text:?}: {err}");
    }
}

/// Blank lines and `#` comments are ours; TLH's files contain neither, so nothing of
/// theirs is misread by allowing them.
#[test]
fn comments_and_blank_lines_are_skipped() {
    let entries = parse_list(
        b"# my list\n\nA|http://a.example/announce\n\n# gone\n",
        Path::new("trackers.lst"),
    )
    .unwrap();
    assert_eq!(entries.len(), 1);
}

/// `Health::usable` is the single gate every refusal goes through, so it is worth pinning
/// which states pass it.
#[test]
fn only_states_we_have_no_evidence_against_are_usable() {
    assert!(Health::Announces.usable());
    assert!(Health::Unreachable.usable());
    assert!(Health::Unchecked.usable());
    assert!(!Health::PersonalUrl.usable());
    assert!(!Health::Broken.usable());
}

/// The type the CLI prints from, kept honest about what it exposes.
#[test]
fn a_bundled_entry_exposes_what_the_listing_needs() {
    let list = TrackerList::bundled();
    let t: &Tracker = list.get("tradersden").unwrap();
    assert_eq!(t.name, "The Traders' Den");
    assert_eq!(t.health, Health::Announces);
    assert_eq!(t.checked.as_deref(), Some("2026-08-30"));
    assert!(t.evidence.as_deref().unwrap().contains("Invalid info_hash"));
}
