//! The spoken lines Spotify's DJ delivers around the tracks of a DJ context.

use std::collections::HashMap;

use crate::{
    SAMPLE_RATE,
    player::{NormalisationData, db_to_ratio},
    protocol::{
        client_tts::TtsRequest,
        tts_resolve::resolve_request::{AudioFormat, TtsProvider, TtsVoice},
    },
};
use protobuf::Enum;

const INTRO_PREFIX: &str = "narration.intro";
const JUMP_PREFIX: &str = "narration.jump";
const OUTRO_PREFIX: &str = "narration.outro";

/// The integrated loudness Spotify normalises to. A track's `NormalisationData` already states
/// its gain relative to this; a narration clip states its absolute loudness instead.
const SPOTIFY_LOUDNESS_TARGET_LUFS: f64 = -14.0;

/// The levels the DJ declares for its own narration, used when a clip declares none. Treating a
/// missing loudness as 0 LUFS would normalise the clip into silence.
const DEFAULT_LOUDNESS_DB: f64 = -16.0;
const DEFAULT_TRUE_PEAK_DB: f64 = -3.0;

/// One spoken line, before it has been synthesized.
#[derive(Debug, Clone, PartialEq)]
pub struct NarrationClip {
    pub ssml: String,
    pub language: String,
    pub voice: TtsVoice,
    pub provider: TtsProvider,
    pub loudness_db: f64,
    pub true_peak_db: f64,
}

impl NarrationClip {
    fn from_metadata(metadata: &HashMap<String, String>, prefix: &str) -> Option<Self> {
        let ssml = metadata.get(&format!("{prefix}.ssml"))?;
        if ssml.is_empty() {
            return None;
        }

        fn enum_value<E: Enum>(metadata: &HashMap<String, String>, key: String, fallback: E) -> E {
            metadata
                .get(&key)
                .and_then(|name| E::from_str(name))
                .unwrap_or(fallback)
        }

        let level = |suffix: &str, fallback| {
            metadata
                .get(&format!("{prefix}.{suffix}"))
                .and_then(|value| value.parse().ok())
                .unwrap_or(fallback)
        };

        Some(Self {
            ssml: ssml.clone(),
            language: metadata
                .get(&format!("{prefix}.language"))
                .cloned()
                .unwrap_or_default(),
            voice: enum_value(metadata, format!("{prefix}.voice"), TtsVoice::VOICE1),
            provider: enum_value(
                metadata,
                format!("{prefix}.tts_provider"),
                TtsProvider::SONANTIC_FAST,
            ),
            loudness_db: level("loudness", DEFAULT_LOUDNESS_DB),
            true_peak_db: level("true_peak", DEFAULT_TRUE_PEAK_DB),
        })
    }

    pub fn tts_request(&self) -> TtsRequest {
        let mut request = TtsRequest::new();
        request.set_ssml(self.ssml.clone());
        request.language = self.language.clone();
        request.audio_format = AudioFormat::MP3.into();
        request.tts_voice = self.voice.into();
        request.tts_provider = self.provider.into();
        // The player has neither a resampler nor a mono upmix, so anything else is unplayable.
        request.sample_rate_hz = SAMPLE_RATE as i32;
        request
    }

    /// The clip's levels expressed the way a track's are, so that the player's own normalisation
    /// settings decide the gain.
    pub fn normalisation_data(&self) -> NormalisationData {
        let gain_db = SPOTIFY_LOUDNESS_TARGET_LUFS - self.loudness_db;
        let peak = db_to_ratio(self.true_peak_db);

        NormalisationData {
            track_gain_db: gain_db,
            track_peak: peak,
            album_gain_db: gain_db,
            album_peak: peak,
        }
    }
}

/// The lines that belong to one track of a DJ context.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackNarration {
    pub intro: Option<NarrationClip>,
    pub outro: Option<NarrationClip>,
}

impl TrackNarration {
    /// Reads a track's narration out of its context metadata.
    ///
    /// `jumped` picks between the two lead-ins a track carries: the jump line is worded for
    /// having moved to the track deliberately, the intro for having arrived at it in turn.
    pub fn from_metadata(metadata: &HashMap<String, String>, jumped: bool) -> Option<Self> {
        let intro_prefix = if jumped { JUMP_PREFIX } else { INTRO_PREFIX };

        let narration = Self {
            intro: NarrationClip::from_metadata(metadata, intro_prefix),
            outro: NarrationClip::from_metadata(metadata, OUTRO_PREFIX),
        };

        (narration.intro.is_some() || narration.outro.is_some()).then_some(narration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_track_without_narration_has_none() {
        assert_eq!(
            TrackNarration::from_metadata(&metadata(&[("media.type", "audio")]), false),
            None
        );
    }

    #[test]
    fn an_empty_script_does_not_count_as_narration() {
        assert_eq!(
            TrackNarration::from_metadata(&metadata(&[("narration.intro.ssml", "")]), false),
            None
        );
    }

    #[test]
    fn jumping_to_a_track_picks_the_jump_line() {
        let metadata = metadata(&[
            ("narration.intro.ssml", "<speak>next up</speak>"),
            ("narration.jump.ssml", "<speak>straight to it</speak>"),
        ]);

        let in_turn = TrackNarration::from_metadata(&metadata, false).expect("narration");
        let jumped = TrackNarration::from_metadata(&metadata, true).expect("narration");

        assert_eq!(in_turn.intro.expect("intro").ssml, "<speak>next up</speak>");
        assert_eq!(
            jumped.intro.expect("intro").ssml,
            "<speak>straight to it</speak>"
        );
    }

    #[test]
    fn an_outro_alone_is_narration() {
        let narration = TrackNarration::from_metadata(
            &metadata(&[("narration.outro.ssml", "<speak/>")]),
            false,
        )
        .expect("narration");

        assert_eq!(narration.intro, None);
        assert!(narration.outro.is_some());
    }

    #[test]
    fn declared_voice_and_levels_are_used() {
        let narration = TrackNarration::from_metadata(
            &metadata(&[
                ("narration.intro.ssml", "<speak/>"),
                ("narration.intro.voice", "VOICE7"),
                ("narration.intro.tts_provider", "CLOUD_TTS"),
                ("narration.intro.loudness", "-12.5"),
                ("narration.intro.true_peak", "-1.25"),
            ]),
            false,
        )
        .expect("narration");

        let intro = narration.intro.expect("intro");
        assert_eq!(intro.voice, TtsVoice::VOICE7);
        assert_eq!(intro.provider, TtsProvider::CLOUD_TTS);
        assert_eq!(intro.loudness_db, -12.5);
        assert_eq!(intro.true_peak_db, -1.25);
    }

    #[test]
    fn missing_levels_fall_back_to_the_declared_defaults() {
        let narration = TrackNarration::from_metadata(
            &metadata(&[("narration.intro.ssml", "<speak/>")]),
            false,
        )
        .expect("narration");

        let intro = narration.intro.expect("intro");
        assert_eq!(intro.loudness_db, DEFAULT_LOUDNESS_DB);
        assert_eq!(intro.true_peak_db, DEFAULT_TRUE_PEAK_DB);
        assert_eq!(intro.voice, TtsVoice::VOICE1);
        assert_eq!(intro.provider, TtsProvider::SONANTIC_FAST);
    }

    #[test]
    fn an_unparseable_level_falls_back_rather_than_silencing_the_clip() {
        let narration = TrackNarration::from_metadata(
            &metadata(&[
                ("narration.intro.ssml", "<speak/>"),
                ("narration.intro.loudness", "quiet"),
            ]),
            false,
        )
        .expect("narration");

        assert_eq!(
            narration.intro.expect("intro").loudness_db,
            DEFAULT_LOUDNESS_DB
        );
    }

    #[test]
    fn the_request_asks_for_mp3_at_the_players_sample_rate() {
        let clip = NarrationClip {
            ssml: "<speak/>".into(),
            language: "en-US".into(),
            voice: TtsVoice::VOICE3,
            provider: TtsProvider::POLLY,
            loudness_db: -16.0,
            true_peak_db: -3.0,
        };

        let request = clip.tts_request();
        assert_eq!(request.ssml(), "<speak/>");
        assert_eq!(request.language, "en-US");
        assert_eq!(
            request.audio_format.enum_value_or_default(),
            AudioFormat::MP3
        );
        assert_eq!(request.tts_voice.enum_value_or_default(), TtsVoice::VOICE3);
        assert_eq!(request.sample_rate_hz, SAMPLE_RATE as i32);
    }
}
