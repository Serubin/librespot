use super::{AudioDecoder, AudioPacket, AudioPacketPosition, DecoderError, DecoderResult};

/// A track with the DJ talking around it: a line before it, a line after it, either of which may
/// be absent.
///
/// The parts play one after another rather than mixed, matching the zero `ms_narration_overlapping`
/// the official client reports. Position stays the track's throughout, so a track reads as not yet
/// started while the DJ introduces it.
pub struct NarratedDecoder {
    /// played in order; `main` is the index of the track among them
    parts: Vec<Part>,
    main: usize,
    current: usize,
    track_duration_ms: u32,
}

struct Part {
    decoder: Box<dyn AudioDecoder + Send>,
    /// applied to every sample of a narration part, so that the one normalisation factor the
    /// player computed for the track lands the clip at the same target
    gain: f64,
}

impl NarratedDecoder {
    pub fn new(
        intro: Option<(Box<dyn AudioDecoder + Send>, f64)>,
        track: Box<dyn AudioDecoder + Send>,
        outro: Option<(Box<dyn AudioDecoder + Send>, f64)>,
        track_duration_ms: u32,
    ) -> Self {
        let mut parts = Vec::with_capacity(3);

        if let Some((decoder, gain)) = intro {
            parts.push(Part { decoder, gain });
        }

        let main = parts.len();
        parts.push(Part {
            decoder: track,
            gain: 1.0,
        });

        if let Some((decoder, gain)) = outro {
            parts.push(Part { decoder, gain });
        }

        Self {
            parts,
            main,
            current: 0,
            track_duration_ms,
        }
    }

    /// The position a narration part reports: the track has not started yet during the lead-in,
    /// and is over during the closing line.
    fn narration_position_ms(&self) -> u32 {
        if self.current < self.main {
            0
        } else {
            self.track_duration_ms
        }
    }
}

impl AudioDecoder for NarratedDecoder {
    /// Seeks the track, abandoning a lead-in still playing: someone who scrubs wants the music.
    /// The closing line still follows, since it belongs to the end of the track.
    ///
    /// Rewinding to the track unconditionally is what makes a seek out of the closing line, or
    /// out of an exhausted stream as repeat-one does, resume the music rather than spin on a
    /// decoder with nothing left to give.
    fn seek(&mut self, position_ms: u32) -> Result<u32, DecoderError> {
        self.current = self.main;

        for part in self.parts.iter_mut().skip(self.main + 1) {
            part.decoder.seek(0)?;
        }

        self.parts[self.main].decoder.seek(position_ms)
    }

    fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
        while self.current < self.parts.len() {
            let is_narration = self.current != self.main;

            match self.parts[self.current].decoder.next_packet() {
                Ok(None) => self.current += 1,
                Ok(Some((position, packet))) => {
                    if !is_narration {
                        return Ok(Some((position, packet)));
                    }

                    let mut packet = packet;
                    if let AudioPacket::Samples(samples) = &mut packet {
                        let gain = self.parts[self.current].gain;
                        for sample in samples.iter_mut() {
                            *sample *= gain;
                        }
                    }

                    return Ok(Some((
                        AudioPacketPosition {
                            position_ms: self.narration_position_ms(),
                            skipped: false,
                            narration: true,
                        },
                        packet,
                    )));
                }
                Err(e) => {
                    // Losing a spoken line is a much smaller problem than losing the music, so
                    // only the track's own failure is an error.
                    if !is_narration {
                        return Err(e);
                    }

                    warn!("Narration playback failed, skipping it: {e}");
                    self.current += 1;
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yields one packet of a single sample per queued value, then ends, or fails once it has
    /// yielded everything queued before the failure.
    struct StubDecoder {
        samples: Vec<f64>,
        fail_after: Option<usize>,
        yielded: usize,
        seeked_to: Option<u32>,
    }

    impl StubDecoder {
        fn new(samples: &[f64]) -> Box<Self> {
            Box::new(Self {
                samples: samples.to_vec(),
                fail_after: None,
                yielded: 0,
                seeked_to: None,
            })
        }

        fn failing(samples: &[f64], fail_after: usize) -> Box<Self> {
            let mut stub = Self::new(samples);
            stub.fail_after = Some(fail_after);
            stub
        }
    }

    impl AudioDecoder for StubDecoder {
        fn seek(&mut self, position_ms: u32) -> Result<u32, DecoderError> {
            self.seeked_to = Some(position_ms);
            self.yielded = 0;
            Ok(position_ms)
        }

        fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
            if self.fail_after == Some(self.yielded) {
                return Err(DecoderError::SymphoniaDecoder("stub failure".into()));
            }

            let sample = match self.samples.get(self.yielded) {
                None => return Ok(None),
                Some(sample) => *sample,
            };

            self.yielded += 1;

            Ok(Some((
                AudioPacketPosition {
                    position_ms: self.yielded as u32,
                    ..Default::default()
                },
                AudioPacket::Samples(vec![sample]),
            )))
        }
    }

    fn drain(decoder: &mut NarratedDecoder) -> Vec<(u32, bool, f64)> {
        let mut packets = Vec::new();

        while let Some((position, packet)) = decoder.next_packet().expect("packet") {
            let sample = packet.samples().expect("samples")[0];
            packets.push((position.position_ms, position.narration, sample));
        }

        packets
    }

    #[test]
    fn the_parts_play_in_order_with_the_tracks_own_position() {
        let mut decoder = NarratedDecoder::new(
            Some((StubDecoder::new(&[1.0]), 0.5)),
            StubDecoder::new(&[2.0, 3.0]),
            Some((StubDecoder::new(&[4.0]), 0.25)),
            90_000,
        );

        assert_eq!(
            drain(&mut decoder),
            vec![
                (0, true, 0.5),
                (1, false, 2.0),
                (2, false, 3.0),
                (90_000, true, 1.0),
            ]
        );
    }

    #[test]
    fn a_track_without_narration_is_passed_through_untouched() {
        let mut decoder = NarratedDecoder::new(None, StubDecoder::new(&[2.0]), None, 90_000);

        assert_eq!(drain(&mut decoder), vec![(1, false, 2.0)]);
    }

    #[test]
    fn a_failing_narration_is_dropped_and_the_music_still_plays() {
        let mut decoder = NarratedDecoder::new(
            Some((StubDecoder::failing(&[1.0], 1), 1.0)),
            StubDecoder::new(&[2.0]),
            None,
            90_000,
        );

        assert_eq!(drain(&mut decoder), vec![(0, true, 1.0), (1, false, 2.0)]);
    }

    #[test]
    fn a_failing_track_is_an_error() {
        let mut decoder = NarratedDecoder::new(None, StubDecoder::failing(&[], 0), None, 90_000);

        assert!(decoder.next_packet().is_err());
    }

    #[test]
    fn seeking_abandons_the_lead_in_but_keeps_the_closing_line() {
        let mut decoder = NarratedDecoder::new(
            Some((StubDecoder::new(&[1.0]), 1.0)),
            StubDecoder::new(&[2.0]),
            Some((StubDecoder::new(&[4.0]), 1.0)),
            90_000,
        );

        assert_eq!(decoder.seek(30_000).expect("seek"), 30_000);
        assert_eq!(
            drain(&mut decoder),
            vec![(1, false, 2.0), (90_000, true, 4.0)]
        );
    }

    #[test]
    fn seeking_out_of_the_closing_line_returns_to_the_music() {
        let mut decoder = NarratedDecoder::new(
            None,
            StubDecoder::new(&[2.0]),
            Some((StubDecoder::new(&[4.0]), 1.0)),
            90_000,
        );

        // Play into the closing line, then scrub back.
        decoder.next_packet().expect("track");
        decoder.next_packet().expect("outro");

        decoder.seek(30_000).expect("seek");

        assert_eq!(
            drain(&mut decoder),
            vec![(1, false, 2.0), (90_000, true, 4.0)]
        );
    }

    #[test]
    fn seeking_an_exhausted_stream_replays_it_as_repeat_one_needs() {
        let mut decoder = NarratedDecoder::new(
            Some((StubDecoder::new(&[1.0]), 1.0)),
            StubDecoder::new(&[2.0]),
            None,
            90_000,
        );

        assert!(!drain(&mut decoder).is_empty());
        assert!(decoder.next_packet().expect("exhausted").is_none());

        decoder.seek(0).expect("seek");

        assert_eq!(drain(&mut decoder), vec![(1, false, 2.0)]);
    }
}
