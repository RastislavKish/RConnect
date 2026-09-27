//! The voices and variants available in `espeak-ng-data`, read from its voice files.
//!
//! Reading the files instead of asking eSpeak NG keeps this usable while the speech
//! engine runs: eSpeak NG is global and not thread safe.

use std::path::Path;

/// A language voice or a variant, as offered in the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceInfo {
    /// What eSpeak NG selects it by: a language code such as `en-us`, or a variant's
    /// file name such as `klatt`.
    pub id: String,
    /// Human readable name, e.g. `English (America)`.
    pub name: String,
}

/// Language voices, sorted by name. `data_dir` contains `espeak-ng-data`.
pub fn list_voices(data_dir: &Path) -> Vec<VoiceInfo> {
    let mut voices = Vec::new();
    collect_voices(&data_dir.join("espeak-ng-data/lang"), &mut voices);
    voices.sort_by_key(|voice| voice.name.to_lowercase());
    voices.dedup_by(|a, b| a.id == b.id);
    voices
}

/// Voice variants (e.g. `klatt`, `m1`), sorted by name.
pub fn list_variants(data_dir: &Path) -> Vec<VoiceInfo> {
    let mut variants: Vec<VoiceInfo> = std::fs::read_dir(data_dir.join("espeak-ng-data/voices/!v"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(entry.path()).ok()?;
            let name = field(&text, "name").unwrap_or(&id).to_string();
            Some(VoiceInfo { id, name })
        })
        .collect();
    variants.sort_by_key(|variant| variant.name.to_lowercase());
    variants
}

fn collect_voices(dir: &Path, voices: &mut Vec<VoiceInfo>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_voices(&path, voices);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            let language =
                field(&text, "language").and_then(|value| value.split_whitespace().next());
            if let (Some(name), Some(language)) = (field(&text, "name"), language) {
                voices.push(VoiceInfo {
                    id: language.to_string(),
                    name: name.to_string(),
                });
            }
        }
    }
}

/// The value of the first `key value` line.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.trim().split_once(char::is_whitespace)?;
        (name == key)
            .then(|| value.trim())
            .filter(|value| !value.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_bundled_voices() {
        let Some(data) = crate::speech::espeak::dev_data() else {
            return;
        };
        let voices = list_voices(data);
        assert!(voices.len() > 100, "{} voices", voices.len());
        assert!(voices.contains(&VoiceInfo {
            id: "sk".into(),
            name: "Slovak".into()
        }));
        assert!(
            voices
                .iter()
                .any(|voice| voice.id == "en-us" && voice.name == "English (America)")
        );

        let variants = list_variants(data);
        assert!(variants.len() > 50, "{} variants", variants.len());
        assert!(variants.contains(&VoiceInfo {
            id: "klatt".into(),
            name: "klatt".into()
        }));
    }

    #[test]
    fn fields() {
        let text = "name English (America)\nlanguage en-us 2\nlanguage en 3\n";
        assert_eq!(field(text, "name"), Some("English (America)"));
        assert_eq!(field(text, "language"), Some("en-us 2"));
        assert_eq!(field(text, "gender"), None);
    }
}
