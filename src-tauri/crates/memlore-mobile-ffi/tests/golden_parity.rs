//! Golden parity through the FFI: the committed v0.1.0 desktop fixture must
//! open through the exported `open_entry_frame` with the same results the
//! engine's `golden_desktop_fixture_decodes` checks. Plus the lock gate of
//! `MemloreEngine`.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use memlore_core::envelope::{
    load_content_list, unwrap_master_with_recovery, verify_master_fingerprint,
};
use memlore_core::metadata::EntryMetadata;
use memlore_mobile_ffi::{
    open_entry_frame, MemloreEngine, MemloreError, SearchFiltersFfi, StartupModeFfi,
};
use serde_json::Value;
use yrs::updates::decoder::Decode;
use yrs::{GetString, ReadTxn, Transact, Update, XmlFragment};

/// Never regenerated (see `memlore-core/fixtures/README.md`).
const FIXTURE_REL_PATH: &str = "../memlore-core/fixtures/desktop-vault.v1.json";

fn unb64(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap()
}

fn yjs_text(update: &[u8]) -> String {
    let doc = yrs::Doc::new();
    let t = doc.get_or_insert_text("content");
    doc.transact_mut()
        .apply_update(Update::decode_v1(update).unwrap())
        .unwrap();
    let s = t.get_string(&doc.transact());
    s
}

#[test]
fn golden_desktop_fixture_opens_through_ffi() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL_PATH);
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let files: BTreeMap<String, Vec<u8>> = fixture["files"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), unb64(v.as_str().unwrap())))
        .collect();
    let device = fixture["device_id"].as_str().unwrap();
    let gen = fixture["generation"].as_u64().unwrap();
    let base = format!("generations/g-{gen}/{device}");
    let json_of = |path: &str| -> Value { serde_json::from_slice(&files[path]).unwrap() };
    let expected = &fixture["expected"];

    // Phrase -> master -> content list, as a joining device does.
    let recovery = json_of(".meta/keyring/_recovery.json");
    let meta = json_of(".meta/keyring/_meta.json");
    let phrase = fixture["recovery_phrase"].as_str().unwrap();
    let master =
        unwrap_master_with_recovery(phrase, recovery["wrapped_master"].as_str().unwrap()).unwrap();
    verify_master_fingerprint(&master, meta["master_fingerprint"].as_str().unwrap()).unwrap();
    let list = load_content_list(
        std::str::from_utf8(&files[".meta/keyring/_content.json"]).unwrap(),
        &master,
    )
    .unwrap();
    let content_key_hex = hex::encode(*list.keys[&list.latest]);

    let expected_entries = expected["entries"].as_array().unwrap();
    assert_eq!(expected_entries.len(), 7);
    let mut first_frame = None;
    for e in expected_entries {
        let id = e["entry_id"].as_str().unwrap();
        let bytes = files[&format!("{base}/entries/{id}.bin")].clone();
        first_frame.get_or_insert_with(|| bytes.clone());
        let opened = open_entry_frame(content_key_hex.clone(), bytes).unwrap();
        let m: EntryMetadata = serde_json::from_str(&opened.metadata_json).unwrap();
        assert_eq!(m.entry_id, id);
        assert_eq!(m.title.as_deref(), e["title"].as_str());
        assert_eq!(m.content_text.as_deref(), e["content_text"].as_str());
        assert_eq!(m.emotion.as_deref(), e["emotion"].as_str());
        assert_eq!(m.is_favorite, e["is_favorite"].as_bool().unwrap());
        assert_eq!(m.is_locked, e["is_locked"].as_bool().unwrap());
        assert_eq!(m.journal_name.as_deref(), e["journal_name"].as_str());
        assert_eq!(m.tag_ids.len(), e["tags"].as_array().unwrap().len());
        assert_eq!(m.media.len(), e["media_ids"].as_array().unwrap().len());
        assert_eq!(yjs_text(&opened.yjs), e["content_text"].as_str().unwrap());
    }

    // Any tampered byte is a format error, never a panic or a partial open:
    // the magic header, then the last byte (inside the sealed metadata).
    let frame = first_frame.unwrap();
    for at in [0, frame.len() - 1] {
        let mut tampered = frame.clone();
        tampered[at] ^= 0x01;
        let err = open_entry_frame(content_key_hex.clone(), tampered).unwrap_err();
        assert!(
            matches!(err, MemloreError::Format { .. }),
            "byte {at}: {err:?}"
        );
    }
    // A key of the wrong length is a format error too.
    let err = open_entry_frame("00".to_string(), frame).unwrap_err();
    assert!(matches!(err, MemloreError::Format { .. }), "{err:?}");
}

/// The FFI surface rule: data calls fail closed while locked, and a wrong
/// password is reported as such, not as a generic error.
#[test]
fn engine_data_calls_are_lock_gated() {
    const PASSWORD: &str = "password12";
    let dir = tempfile::TempDir::new().unwrap();
    let app_dir = dir.path().to_string_lossy().into_owned();

    let engine = MemloreEngine::new(app_dir.clone(), "0.0.0-test".to_string()).unwrap();
    assert_eq!(engine.startup_mode(), StartupModeFfi::FirstLaunch);
    assert!(matches!(engine.entry_count(), Err(MemloreError::Locked)));
    assert!(matches!(
        engine.create_entry("t".to_string(), None),
        Err(MemloreError::Locked)
    ));
    assert!(matches!(
        engine.list_entries(None, 0),
        Err(MemloreError::Locked)
    ));
    // A lock before setup (auto-lock on the onboarding screen) must leave the
    // vault creatable: there is no vault yet, so nothing is swapped out.
    engine.lock().unwrap();

    let phrase = engine.create_vault(PASSWORD.to_string()).unwrap();
    assert_eq!(phrase.split_whitespace().count(), 24);
    let id = engine.create_entry("First".to_string(), None).unwrap();
    assert_eq!(engine.entry_count().unwrap(), 1);

    // Every entry, journal, tag, trash, search, version and template call
    // must return `Locked` too. Made with real ids, so a call that skipped
    // the gate would succeed.
    struct Ids {
        entry: String,
        journal: String,
        spare_journal: String,
        tag: String,
        version: String,
        template: String,
    }
    type Call = fn(&MemloreEngine, &Ids) -> Result<(), MemloreError>;
    // `lock_second_lock` and `close_invisible_vault` are not listed: they are
    // infallible and data-free (they only end the in-memory lock session).
    let calls: [(&str, Call); 47] = [
        ("list_entries", |e, ids| {
            e.list_entries(Some(ids.journal.clone()), 0).map(drop)
        }),
        ("get_entry", |e, ids| {
            e.get_entry(ids.entry.clone()).map(drop)
        }),
        ("create_entry", |e, ids| {
            e.create_entry("t".to_string(), Some(ids.journal.clone()))
                .map(drop)
        }),
        ("set_entry_title", |e, ids| {
            e.set_entry_title(ids.entry.clone(), "t".to_string())
        }),
        ("set_entry_emotion", |e, ids| {
            e.set_entry_emotion(ids.entry.clone(), Some("good".to_string()))
        }),
        ("toggle_entry_favorite", |e, ids| {
            e.toggle_entry_favorite(ids.entry.clone()).map(drop)
        }),
        ("set_entry_date", |e, ids| {
            e.set_entry_date(ids.entry.clone(), 86_400)
        }),
        ("move_entry", |e, ids| {
            e.move_entry(ids.entry.clone(), ids.journal.clone())
        }),
        ("trash_entry", |e, ids| e.trash_entry(ids.entry.clone())),
        ("list_trash", |e, _| e.list_trash().map(drop)),
        ("restore_entry", |e, ids| e.restore_entry(ids.entry.clone())),
        ("purge_entry", |e, ids| e.purge_entry(ids.entry.clone())),
        ("empty_trash", |e, ids| {
            e.empty_trash(vec![ids.entry.clone()]).map(drop)
        }),
        ("list_journals", |e, _| e.list_journals().map(drop)),
        ("create_journal", |e, _| {
            e.create_journal("j".to_string()).map(drop)
        }),
        ("rename_journal", |e, ids| {
            e.rename_journal(ids.journal.clone(), "j".to_string())
                .map(drop)
        }),
        ("list_tags", |e, _| e.list_tags().map(drop)),
        ("create_tag", |e, _| e.create_tag("t".to_string()).map(drop)),
        ("add_tag_to_entry", |e, ids| {
            e.add_tag_to_entry(ids.entry.clone(), ids.tag.clone())
        }),
        ("remove_tag_from_entry", |e, ids| {
            e.remove_tag_from_entry(ids.entry.clone(), ids.tag.clone())
        }),
        ("rename_tag", |e, ids| {
            e.rename_tag(ids.tag.clone(), "r".to_string()).map(drop)
        }),
        ("delete_tag", |e, ids| e.delete_tag(ids.tag.clone())),
        ("journal_entry_count", |e, ids| {
            e.journal_entry_count(ids.journal.clone()).map(drop)
        }),
        ("delete_journal", |e, ids| {
            e.delete_journal(ids.spare_journal.clone())
        }),
        ("search_entries", |e, _| {
            e.search_entries("First".to_string(), SearchFiltersFfi::default())
                .map(drop)
        }),
        ("list_entry_versions", |e, ids| {
            e.list_entry_versions(ids.entry.clone()).map(drop)
        }),
        ("get_entry_version_content", |e, ids| {
            e.get_entry_version_content(ids.entry.clone(), ids.version.clone())
                .map(drop)
        }),
        ("snapshot_entry_version", |e, ids| {
            e.snapshot_entry_version(ids.entry.clone()).map(drop)
        }),
        ("list_templates", |e, _| e.list_templates().map(drop)),
        ("create_template", |e, _| {
            e.create_template("t".to_string(), None).map(drop)
        }),
        ("update_template", |e, ids| {
            e.update_template(ids.template.clone(), "t".to_string(), None)
                .map(drop)
        }),
        ("delete_template", |e, ids| {
            e.delete_template(ids.template.clone())
        }),
        ("lock_state", |e, _| e.lock_state().map(drop)),
        ("set_second_lock_password", |e, _| {
            e.set_second_lock_password("lock-pw-1".to_string())
                .map(drop)
        }),
        ("unlock_second_lock", |e, _| {
            e.unlock_second_lock("lock-pw-1".to_string()).map(drop)
        }),
        ("change_second_lock_password", |e, _| {
            e.change_second_lock_password("lock-pw-1".to_string(), "lock-pw-2".to_string())
                .map(drop)
        }),
        ("disable_second_lock", |e, _| {
            e.disable_second_lock("lock-pw-1".to_string()).map(drop)
        }),
        ("set_second_lock_show_existence", |e, _| {
            e.set_second_lock_show_existence(true).map(drop)
        }),
        ("set_second_lock_auto_lock_minutes", |e, _| {
            e.set_second_lock_auto_lock_minutes(15).map(drop)
        }),
        ("set_invisible_auto_lock_minutes", |e, _| {
            e.set_invisible_auto_lock_minutes(15).map(drop)
        }),
        ("set_entry_locked", |e, ids| {
            e.set_entry_locked(ids.entry.clone(), true).map(drop)
        }),
        ("set_journal_locked", |e, ids| {
            e.set_journal_locked(ids.journal.clone(), true).map(drop)
        }),
        ("open_invisible_vault", |e, _| {
            e.open_invisible_vault("vault-pw-1".to_string()).map(drop)
        }),
        ("change_invisible_vault_password", |e, _| {
            e.change_invisible_vault_password("vault-pw-1".to_string(), "vault-pw-2".to_string())
                .map(drop)
        }),
        ("remove_empty_invisible_vaults", |e, _| {
            e.remove_empty_invisible_vaults().map(drop)
        }),
        ("set_entry_invisible", |e, ids| {
            e.set_entry_invisible(ids.entry.clone(), true).map(drop)
        }),
        ("set_journal_invisible", |e, ids| {
            e.set_journal_invisible(ids.journal.clone(), true).map(drop)
        }),
    ];
    let base = engine.load_entry_yjs(id.clone()).unwrap();
    engine
        .save_entry_yjs(
            id.clone(),
            edit_and_encode(&base, 9, "gated"),
            "gated".to_string(),
            "gated".to_string(),
        )
        .unwrap();
    let ids = Ids {
        entry: id.clone(),
        journal: engine.get_entry(id.clone()).unwrap().journal_id,
        spare_journal: engine.create_journal("Spare".to_string()).unwrap().id,
        tag: engine.create_tag("gated".to_string()).unwrap().id,
        version: engine.snapshot_entry_version(id.clone()).unwrap().unwrap(),
        template: engine
            .create_template("gated".to_string(), None)
            .unwrap()
            .id,
    };

    engine.lock().unwrap();
    assert!(matches!(engine.entry_count(), Err(MemloreError::Locked)));
    assert!(matches!(
        engine.list_entries(None, 0),
        Err(MemloreError::Locked)
    ));
    assert!(matches!(
        engine.load_entry_yjs(id.clone()),
        Err(MemloreError::Locked)
    ));
    assert!(matches!(
        engine.save_entry_yjs(id.clone(), Vec::new(), String::new(), String::new()),
        Err(MemloreError::Locked)
    ));
    for (name, call) in calls {
        let result = call(&engine, &ids);
        assert!(
            matches!(result, Err(MemloreError::Locked)),
            "{name}: {result:?}"
        );
    }
    // The lock closed the database; the password opens it again in place.
    engine.unlock(PASSWORD.to_string()).unwrap();
    assert_eq!(engine.entry_count().unwrap(), 1);
    engine.lock().unwrap();

    // A fresh engine on the same vault starts locked, as after an app restart.
    drop(engine);
    let engine = MemloreEngine::new(app_dir, "0.0.0-test".to_string()).unwrap();
    assert_eq!(engine.startup_mode(), StartupModeFfi::PasswordLocked);
    assert!(matches!(engine.entry_count(), Err(MemloreError::Locked)));
    assert!(matches!(
        engine.unlock("wrong-password".to_string()),
        Err(MemloreError::WrongPassword)
    ));
    assert!(matches!(engine.entry_count(), Err(MemloreError::Locked)));

    engine.unlock(PASSWORD.to_string()).unwrap();
    let rows = engine.list_entries(None, 0).unwrap().items;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].title.as_deref(), Some("First"));
}

/// Paragraph texts of a TipTap entry doc (fragment `default`).
fn paragraphs(update: &[u8]) -> Vec<String> {
    let doc = yrs::Doc::new();
    let fragment = doc.get_or_insert_xml_fragment("default");
    doc.transact_mut()
        .apply_update(Update::decode_v1(update).unwrap())
        .unwrap();
    let txn = doc.transact();
    fragment
        .children(&txn)
        .filter_map(|child| match child {
            yrs::types::xml::XmlOut::Element(el) => Some(el.get_string(&txn)),
            _ => None,
        })
        .collect()
}

/// An editor that opened the entry, then saved the full state.
fn edit_and_encode(base: &[u8], client_id: u64, text: &str) -> Vec<u8> {
    use yrs::{XmlElementPrelim, XmlFragment, XmlTextPrelim};
    let doc = yrs::Doc::with_client_id(client_id);
    let fragment = doc.get_or_insert_xml_fragment("default");
    let mut txn = doc.transact_mut();
    txn.apply_update(Update::decode_v1(base).unwrap()).unwrap();
    let paragraph = fragment.push_back(&mut txn, XmlElementPrelim::new("paragraph", []));
    paragraph.push_back(&mut txn, XmlTextPrelim::new(text));
    txn.encode_state_as_update_v1(&yrs::StateVector::default())
}

/// The mobile save is the desktop's merged save, never the replace
/// primitive: a full state that misses an edit stored meanwhile (another
/// editor, an MCP append) must not drop that edit.
#[test]
fn save_entry_yjs_merges_into_the_stored_doc() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = MemloreEngine::new(
        dir.path().to_string_lossy().into_owned(),
        "0.0.0-test".into(),
    )
    .unwrap();
    engine.create_vault("password12".to_string()).unwrap();
    let id = engine.create_entry("Merge".to_string(), None).unwrap();
    let base = engine.load_entry_yjs(id.clone()).unwrap();
    assert!(!base.is_empty());

    let other = edit_and_encode(&base, 1, "stored meanwhile");
    engine
        .save_entry_yjs(id.clone(), other, "x".into(), "x".into())
        .unwrap();
    let mobile = edit_and_encode(&base, 2, "mobile edit");
    engine
        .save_entry_yjs(id.clone(), mobile, "x".into(), "x".into())
        .unwrap();

    let stored = paragraphs(&engine.load_entry_yjs(id.clone()).unwrap());
    assert!(
        stored.iter().any(|p| p.contains("stored meanwhile")),
        "{stored:?}"
    );
    assert!(
        stored.iter().any(|p| p.contains("mobile edit")),
        "{stored:?}"
    );

    assert!(matches!(
        engine.load_entry_yjs("no-such-entry".to_string()),
        Err(MemloreError::NotFound)
    ));
    assert!(matches!(
        engine.save_entry_yjs(
            "no-such-entry".into(),
            Vec::new(),
            String::new(),
            String::new()
        ),
        Err(MemloreError::NotFound)
    ));
}

/// Writes `mobile/editor/__fixtures__/desktop-vault-yjs.json`: the Yjs body
/// of every entry in the frozen desktop fixture, base64, for the mobile
/// editor's "opening never mutates" test. Those bodies are the sync-helper
/// shape (`Y.Text` "content", no TipTap `default` fragment). Plus the empty
/// body `create_entry` writes. Run once, only to regenerate deliberately:
///
/// `MEMLORE_REGEN_FIXTURES=1 cargo test -p memlore-mobile-ffi write_mobile_editor_vault_fixture -- --ignored`
#[test]
#[ignore = "fixture generator; see the doc comment"]
fn write_mobile_editor_vault_fixture() {
    if std::env::var("MEMLORE_REGEN_FIXTURES").as_deref() != Ok("1") {
        return;
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL_PATH);
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let file = |p: &str| unb64(fixture["files"][p].as_str().unwrap());
    let json_of = |p: &str| -> Value { serde_json::from_slice(&file(p)).unwrap() };
    let master = unwrap_master_with_recovery(
        fixture["recovery_phrase"].as_str().unwrap(),
        json_of(".meta/keyring/_recovery.json")["wrapped_master"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let list = load_content_list(
        std::str::from_utf8(&file(".meta/keyring/_content.json")).unwrap(),
        &master,
    )
    .unwrap();
    let key_hex = hex::encode(*list.keys[&list.latest]);
    let base = format!(
        "generations/g-{}/{}",
        fixture["generation"].as_u64().unwrap(),
        fixture["device_id"].as_str().unwrap()
    );

    let mut entries = serde_json::Map::new();
    for e in fixture["expected"]["entries"].as_array().unwrap() {
        let id = e["entry_id"].as_str().unwrap();
        let opened =
            open_entry_frame(key_hex.clone(), file(&format!("{base}/entries/{id}.bin"))).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&opened.yjs);
        entries.insert(id.to_string(), Value::String(b64));
    }
    let out = serde_json::json!({
        "_generated": "MEMLORE_REGEN_FIXTURES=1 cargo test -p memlore-mobile-ffi \
            write_mobile_editor_vault_fixture -- --ignored (from \
            memlore-core/fixtures/desktop-vault.v1.json); do not edit by hand",
        "entries": entries,
        // The body `create_entry` writes (every entry made on mobile opens as this).
        "created_empty": base64::engine::general_purpose::STANDARD
            .encode(memlore_engine::yjs_doc::build_entry_yjs(&[""], &[]).0),
    });
    let dest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../mobile/editor/__fixtures__/desktop-vault-yjs.json");
    std::fs::write(dest, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
}
