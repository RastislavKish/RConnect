//! Parsing of speech-dispatcher's `SET` parameters into [`SpeechSettings`].

use rconnect_protocol::messages::{CapitalLetters, Punctuation, SpeechSettings};

/// Applies one `name=value` line of a `SET` block. Unknown names are ignored.
pub fn apply(settings: &mut SpeechSettings, line: &str) {
    let Some((name, value)) = line.trim_end_matches('\n').split_once('=') else {
        return;
    };
    let text = || (value != "NULL" && !value.is_empty()).then(|| value.to_string());
    let number = || value.parse::<i32>().ok().map(|n| n.clamp(-100, 100) as i8);

    match name {
        "rate" => settings.rate = number().unwrap_or(settings.rate),
        "pitch" => settings.pitch = number().unwrap_or(settings.pitch),
        "pitch_range" => settings.pitch_range = number().unwrap_or(settings.pitch_range),
        "volume" => settings.volume = number().unwrap_or(settings.volume),
        "punctuation_mode" => {
            settings.punctuation = match value {
                "none" => Punctuation::None,
                "some" => Punctuation::Some,
                "most" => Punctuation::Most,
                "all" => Punctuation::All,
                _ => settings.punctuation,
            }
        }
        "spelling_mode" => settings.spelling = value == "on",
        "cap_let_recogn" => {
            settings.capital_letters = match value {
                "none" => CapitalLetters::None,
                "spell" => CapitalLetters::Spell,
                "icon" => CapitalLetters::Icon,
                _ => settings.capital_letters,
            }
        }
        "voice" => settings.voice_type = text(),
        "synthesis_voice" => settings.synthesis_voice = text(),
        "language" => settings.language = text(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_known_parameters() {
        let mut settings = SpeechSettings::default();
        for line in [
            "rate=40\n",
            "pitch=-10\n",
            "pitch_range=500\n",
            "punctuation_mode=all\n",
            "spelling_mode=on\n",
            "cap_let_recogn=icon\n",
            "voice=MALE1\n",
            "language=sk\n",
            "synthesis_voice=NULL\n",
            "unknown=1\n",
            "garbage\n",
        ] {
            apply(&mut settings, line);
        }
        assert_eq!(settings.rate, 40);
        assert_eq!(settings.pitch, -10);
        assert_eq!(settings.pitch_range, 100);
        assert_eq!(settings.punctuation, Punctuation::All);
        assert!(settings.spelling);
        assert_eq!(settings.capital_letters, CapitalLetters::Icon);
        assert_eq!(settings.voice_type.as_deref(), Some("MALE1"));
        assert_eq!(settings.language.as_deref(), Some("sk"));
        assert_eq!(settings.synthesis_voice, None);
    }
}
