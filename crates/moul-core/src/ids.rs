//! Ids and times in the shapes the Go engine uses.

use crate::host::Host;

const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// `len` random letters and digits, drawn without bias.
pub fn random_chars(host: &mut dyn Host, len: usize) -> String {
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 32];
    while out.len() < len {
        host.fill_random(&mut buf);
        for b in buf {
            // 248 is the largest multiple of 62 below 256.
            if b < 248 && out.len() < len {
                out.push(ID_CHARS[(b % 62) as usize] as char);
            }
        }
    }
    out
}

/// A collection id: 15 letters and digits, like PocketBase and the Go engine.
pub fn collection_id(host: &mut dyn Host) -> String {
    random_chars(host, 15)
}

/// A field id: `fld_` and 8 letters and digits.
pub fn field_id(host: &mut dyn Host) -> String {
    format!("fld_{}", random_chars(host, 8))
}

/// A record id: the singular of the collection name, a dash and 15 letters
/// and digits, such as `post-2xMlWP1yDVqDSdr`.
pub fn record_id(host: &mut dyn Host, collection: &str) -> String {
    let prefix = singularize(collection);
    let random = random_chars(host, 15);
    if prefix.is_empty() {
        random
    } else {
        format!("{prefix}-{random}")
    }
}

/// `n` random bytes as lowercase hex.
pub fn random_hex(host: &mut dyn Host, n: usize) -> String {
    let mut buf = vec![0u8; n];
    host.fill_random(&mut buf);
    hex(&buf)
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// The lowercase singular of an English collection name, for record id
/// prefixes: `posts` → `post`, `categories` → `category`, `people` →
/// `person`. Words already singular stay as they are. This covers the
/// common rules of the dictionary the Go engine uses; a rare word may get a
/// different prefix, which changes nothing but the look of new ids.
pub fn singularize(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return lower;
    }
    // Inflect only the last word of snake_case names: `bg_tasks` → `bg_task`.
    let (head, word) = match lower.rfind('_') {
        Some(i) => (&lower[..=i], &lower[i + 1..]),
        None => ("", lower.as_str()),
    };
    format!("{head}{}", singular_word(word))
}

fn singular_word(w: &str) -> String {
    const UNCOUNTABLE: &[&str] = &[
        "data",
        "metadata",
        "analytics",
        "media",
        "news",
        "series",
        "species",
        "information",
        "equipment",
        "feedback",
        "staff",
        "sheep",
        "fish",
        "deer",
        "moose",
        "aircraft",
        "software",
        "hardware",
        "rice",
        "money",
        "music",
        "traffic",
        "weather",
        "luggage",
        "shared",
        "status",
        "chassis",
        "police",
        "jeans",
        "settings",
    ];
    const IRREGULAR: &[(&str, &str)] = &[
        ("people", "person"),
        ("men", "man"),
        ("women", "woman"),
        ("children", "child"),
        ("mice", "mouse"),
        ("geese", "goose"),
        ("teeth", "tooth"),
        ("feet", "foot"),
        ("oxen", "ox"),
        ("dice", "die"),
        ("indices", "index"),
        ("vertices", "vertex"),
        ("matrices", "matrix"),
        ("criteria", "criterion"),
        ("phenomena", "phenomenon"),
        ("analyses", "analysis"),
        ("theses", "thesis"),
        ("crises", "crisis"),
        ("quizzes", "quiz"),
        ("movies", "movie"),
        ("cookies", "cookie"),
        ("pies", "pie"),
        ("ties", "tie"),
        ("lies", "lie"),
        ("zombies", "zombie"),
        ("selfies", "selfie"),
        ("species", "species"),
        ("buses", "bus"),
        ("statuses", "status"),
        ("viruses", "virus"),
        ("campuses", "campus"),
        ("bonuses", "bonus"),
        ("lives", "life"),
        ("wives", "wife"),
        ("knives", "knife"),
        ("leaves", "leaf"),
        ("halves", "half"),
        ("wolves", "wolf"),
        ("shelves", "shelf"),
        ("calves", "calf"),
        ("thieves", "thief"),
        ("loaves", "loaf"),
        ("selves", "self"),
        ("elves", "elf"),
        ("heroes", "hero"),
        ("potatoes", "potato"),
        ("tomatoes", "tomato"),
        ("echoes", "echo"),
        ("vetoes", "veto"),
    ];
    if UNCOUNTABLE.contains(&w) {
        return w.to_owned();
    }
    if let Some((_, s)) = IRREGULAR.iter().find(|(p, _)| *p == w) {
        return (*s).to_owned();
    }
    if let Some(stem) = w.strip_suffix("ies")
        && stem.len() > 1
    {
        return format!("{stem}y");
    }
    for suffix in ["sses", "shes", "ches", "xes", "zzes"] {
        if let Some(stem) = w.strip_suffix(suffix) {
            return format!("{stem}{}", &suffix[..suffix.len() - 2]);
        }
    }
    if w.ends_with("ss") || w.ends_with("us") || w.ends_with("is") || w.len() < 3 {
        return w.to_owned();
    }
    match w.strip_suffix('s') {
        Some(stem) => stem.to_owned(),
        None => w.to_owned(),
    }
}

/// A request id the engine makes for a write that came without one: this
/// device's id, a colon and its next counter value.
pub fn request_id(device: &str, counter: u64) -> String {
    format!("{device}:{counter}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SeededHost;

    #[test]
    fn singulars() {
        for (plural, singular) in [
            ("posts", "post"),
            ("users", "user"),
            ("categories", "category"),
            ("bg_tasks", "bg_task"),
            ("app_events", "app_event"),
            ("people", "person"),
            ("shared", "shared"),
            ("addresses", "address"),
            ("boxes", "box"),
            ("status", "status"),
            ("Users", "user"),
            ("data", "data"),
            ("person", "person"),
            ("movies", "movie"),
        ] {
            assert_eq!(singularize(plural), singular, "{plural}");
        }
    }

    #[test]
    fn id_shapes() {
        let mut h = SeededHost::new(1, 0);
        let c = collection_id(&mut h);
        assert_eq!(c.len(), 15);
        assert!(c.bytes().all(|b| b.is_ascii_alphanumeric()));
        let r = record_id(&mut h, "posts");
        assert!(r.starts_with("post-") && r.len() == 20, "{r}");
        assert!(moul_store::schema::is_valid_field_id(&field_id(&mut h)));
    }
}
