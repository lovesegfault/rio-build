//! Root-table query API: enumeration and the last-use clock, across
//! both the own-roots overlay and a reloaded index view.

use rio_packstore::{Kind, Options, PackStore};

#[test]
fn root_names_and_last_use_span_own_and_loaded_views() {
    let tmp = tempfile::tempdir().unwrap();

    // Process 1: two roots with distinct clocks, flushed to the index.
    {
        let mut store = PackStore::open(tmp.path(), Options::default()).unwrap();
        let a = store.put(Kind(0), b"blob-a").unwrap();
        let b = store.put(Kind(0), b"blob-b").unwrap();
        store.add_root_at("aaaa-old", &[a], 1_000).unwrap();
        store.add_root_at("bbbb-new", &[b], 2_000).unwrap();
        store.flush().unwrap();
    }

    // Process 2: sees the flushed roots, adds an own (unflushed) one.
    let mut store = PackStore::open(tmp.path(), Options::default()).unwrap();
    assert_eq!(store.root_last_use("aaaa-old"), Some(1_000));
    assert_eq!(store.root_last_use("bbbb-new"), Some(2_000));
    assert_eq!(store.root_last_use("cccc-missing"), None);

    let c = store.put(Kind(0), b"blob-c").unwrap();
    store.add_root_at("cccc-own", &[c], 3_000).unwrap();
    let names = store.root_names();
    assert_eq!(names, vec!["aaaa-old", "bbbb-new", "cccc-own"]);

    // touch_root advances the clock; own entry wins over the view.
    assert!(store.touch_root("aaaa-old").unwrap());
    assert!(store.root_last_use("aaaa-old").unwrap() > 1_000);

    // root_names dedups a root present in both views.
    assert_eq!(store.root_names().len(), 3);

    // has_root spans both views without cloning digest lists.
    assert!(store.has_root("aaaa-old"));
    assert!(store.has_root("cccc-own"));
    assert!(!store.has_root("cccc-missing"));

    // kind_of answers from the loaded view and the own overlay alike.
    let a = store.root_digests("aaaa-old").unwrap()[0];
    assert_eq!(store.kind_of(&a), Some(Kind(0)));
    assert_eq!(store.kind_of(&c), Some(Kind(0)));
    let d = store.put(Kind(2), b"blob-d").unwrap();
    assert_eq!(store.kind_of(&d), Some(Kind(2)));
    assert_eq!(
        store.kind_of(&rio_packstore::Digest::of(b"never-stored")),
        None
    );
}
