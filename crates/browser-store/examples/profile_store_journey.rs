use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use browser_store::ProfileStore;

fn temporary_profile_path(label: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(std::env::temp_dir().join(format!(
        "rwb-profile-store-{label}-{}-{nonce}.redb",
        process::id()
    )))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const APP_ORIGIN: &str = "https://app.example";
    const OTHER_ORIGIN: &str = "https://other.example";

    let work_path = temporary_profile_path("work")?;
    let personal_path = temporary_profile_path("personal")?;

    let work_entries = BTreeMap::from([
        (
            "draft".to_owned(),
            "Ada λ
second line"
                .to_owned(),
        ),
        ("theme".to_owned(), "dark".to_owned()),
    ]);
    let other_origin_entries = BTreeMap::from([("theme".to_owned(), "origin-specific".to_owned())]);
    {
        let work = ProfileStore::open(&work_path)?;
        work.replace_local_storage(APP_ORIGIN, &work_entries)?;
        work.replace_local_storage(OTHER_ORIGIN, &other_origin_entries)?;
    }

    {
        let reopened_work = ProfileStore::open(&work_path)?;
        if reopened_work.load_local_storage(APP_ORIGIN)? != work_entries {
            return Err("work profile did not survive close/reopen".into());
        }
        if reopened_work.load_local_storage(OTHER_ORIGIN)? != other_origin_entries {
            return Err("origin-specific state did not survive close/reopen".into());
        }
        if !reopened_work
            .load_local_storage("https://missing.example")?
            .is_empty()
        {
            return Err("missing origin did not read as empty".into());
        }
    }

    let personal_entries = BTreeMap::from([("theme".to_owned(), "personal".to_owned())]);
    {
        let personal = ProfileStore::open(&personal_path)?;
        if !personal.load_local_storage(APP_ORIGIN)?.is_empty() {
            return Err("new personal profile observed work-profile state".into());
        }
        personal.replace_local_storage(APP_ORIGIN, &personal_entries)?;
    }

    {
        let reopened_work = ProfileStore::open(&work_path)?;
        let reopened_personal = ProfileStore::open(&personal_path)?;
        if reopened_work.load_local_storage(APP_ORIGIN)? != work_entries {
            return Err("personal-profile mutation changed work-profile state".into());
        }
        if reopened_personal.load_local_storage(APP_ORIGIN)? != personal_entries {
            return Err("personal profile did not retain its own state".into());
        }
    }

    fs::remove_file(&work_path)?;
    fs::remove_file(&personal_path)?;
    println!(
        "profile-store-journey: close/reopen, origin scoping, and two-profile isolation succeeded"
    );
    Ok(())
}
