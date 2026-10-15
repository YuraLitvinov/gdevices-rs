//! A compiled catalogue in the bound text domain translates `tr`; untranslated text stays English.
use std::process::Command;

#[test]
fn catalogue_translates_known_strings_only() {
    let locale = String::from_utf8(Command::new("locale").arg("-a").output().expect("locale -a").stdout).unwrap();
    let Some(utf8) = locale.lines().find(|l| l.to_lowercase().ends_with("utf8") && !l.starts_with("C.") && *l != "C" && *l != "POSIX") else {
        println!("skip: no non-C UTF-8 locale installed, so gettext cannot select a language");
        return;
    };
    let dir = std::env::temp_dir().join(format!("qldm-i18n-{}", std::process::id()));
    let lang = dir.join("xx/LC_MESSAGES");
    std::fs::create_dir_all(&lang).unwrap();
    let po = dir.join("xx.po");
    std::fs::write(&po, "msgid \"\"\nmsgstr \"Content-Type: text/plain; charset=UTF-8\\n\"\n\nmsgid \"Hostname\"\nmsgstr \"Nom d'hôte\"\n").unwrap();
    let status = Command::new("msgfmt").arg(&po).arg("-o").arg(lang.join("qlinuxdevicemanager.mo")).status();
    if !status.is_ok_and(|s| s.success()) {
        println!("skip: msgfmt is not installed");
        return;
    }
    // gettext reads the environment at setlocale time, so run the probe in a fresh process.
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args(["--exact", "probe", "--nocapture", "--ignored"])
        .env("QLDM_LOCALE_DIR", &dir)
        .env("LC_ALL", utf8)
        .env("LANGUAGE", "xx")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("hostname=Nom d'hôte"), "{text}");
    assert!(text.contains("other=Uptime"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore]
fn probe() {
    qldm_core::i18n::init();
    println!("hostname={}", qldm_core::i18n::tr("Hostname"));
    println!("other={}", qldm_core::i18n::tr("Uptime"));
}
