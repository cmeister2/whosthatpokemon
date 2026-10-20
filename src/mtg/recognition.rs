//! Shared OCR and orientation sequence used by native identification and browser frames.

use image::DynamicImage;
use serde::Serialize;

use super::{
    evidence::{FrameLayout, TextObservations},
    identify::FieldIdentification,
    ocr::MagicOcr,
};
use crate::matching::CandidateOptions;
use crate::{CardMetadata, Error, clock::Instant, mtg};

/// Per-card elapsed times, excluding shared index and model initialization.
#[derive(Serialize)]
pub struct IdentifyCardTimings {
    /// Detector-free title and footer OCR across attempted orientations, in milliseconds.
    pub ocr_ms: f64,
    /// Title reading across attempted orientations, in milliseconds.
    pub title_ocr_ms: f64,
    /// Collector-bar reading across attempted orientations, in milliseconds.
    pub footer_ocr_ms: f64,
    /// Indexed retrieval, candidate-only visual checks and fusion, in milliseconds.
    pub matching_ms: f64,
    /// Total elapsed work for this extracted crop in milliseconds.
    pub total_ms: f64,
}

/// One explicit card orientation and its title/footer-scoped identification.
#[derive(Serialize)]
pub struct IdentificationAttempt {
    /// Literal detector-free OCR evidence for this title crop.
    pub ocr: mtg::ocr::TitleOcrEvidence,
    /// Literal collector-bar rows read on every attempted orientation.
    pub footer_ocr: mtg::ocr::FooterOcrEvidence,
    /// Indexed title/footer proposals and candidate-only visual evidence.
    pub identification: mtg::identify::FieldIdentification,
    /// Failed OCR-aligned verification before a broader reference-alignment retry.
    ///
    /// Retaining this result distinguishes matching a differently stored reference
    /// from changing the observed text orientation. Present only after both fast
    /// OCR orientations abstained and the caller allowed both reference rotations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restricted_reference_alignment: Option<mtg::identify::FieldIdentification>,
    /// Combined title and footer recognizer time for this orientation.
    pub ocr_ms: f64,
    /// Title recognizer time for this orientation.
    pub title_ocr_ms: f64,
    /// Collector-bar recognizer time for this orientation.
    pub footer_ocr_ms: f64,
    /// Retrieval, sparse candidate loading, geometry and fusion elapsed time.
    pub matching_ms: f64,
}

/// A source outline and one or two explicitly oriented title/footer attempts.
#[derive(Serialize)]
pub struct IdentifiedCard {
    /// Corners in oriented source-image coordinates.
    pub corners: [[f32; 2]; 4],
    /// Source outline area in square pixels.
    pub area: f32,
    /// Attempt chosen for the caller-facing provisional identity.
    pub selected_attempt: usize,
    /// Whether strong title or collector proposals disagree between tried orientations.
    pub orientation_conflict: bool,
    /// First expected orientation and, only after failure, its 180-degree opposite.
    pub attempts: Vec<IdentificationAttempt>,
    /// Actual elapsed crop processing times for this invocation.
    pub timings: IdentifyCardTimings,
}

/// Read title and footer, verify candidates, and retry the opposite orientation after abstention.
///
/// Both hosts supply the same bounded matcher and evidence policy through
/// `identify`. OCR, attempt ordering, conflict reporting and raw provenance are shared.
///
/// # Errors
///
/// Returns errors for invalid crop/orientation, OCR failure or candidate matching.
#[allow(clippy::too_many_arguments)]
pub fn identify_crop(
    engine: &MagicOcr,
    image: &DynamicImage,
    layout: FrameLayout,
    rotation_degrees: u16,
    corners: [[f32; 2]; 4],
    area: f32,
    candidate_options: &CandidateOptions,
    mut identify: impl FnMut(&TextObservations, &CandidateOptions) -> Result<FieldIdentification, Error>,
) -> Result<IdentifiedCard, Error> {
    let mut work =
        CropIdentification::new(layout, rotation_degrees, corners, area, candidate_options);
    work.advance(engine, image, &mut identify)
}

/// Retained raw OCR and cumulative matching work for one orientation.
struct PreparedAttempt {
    /// Literal OCR evidence retained across reference-image downloads.
    ocr: mtg::ocr::TitleOcrEvidence,
    /// Literal collector-bar evidence retained across downloads.
    footer_ocr: mtg::ocr::FooterOcrEvidence,
    /// Actual title recognition time.
    title_ms: f64,
    /// Actual footer recognition time.
    footer_ms: f64,
    /// Active matching work including incomplete cold-cache planning passes.
    matching_ms: f64,
}

/// Resumable shared crop operation; a missing-reference error retains completed OCR.
pub(crate) struct CropIdentification {
    /// Caller-selected field geometry.
    layout: FrameLayout,
    /// Expected artwork orientation, followed by its opposite after abstention.
    rotation: u16,
    /// Original source detection coordinates.
    corners: [[f32; 2]; 4],
    /// Original source detection area.
    area: f32,
    /// Shared descriptor/geometry work budgets.
    options: CandidateOptions,
    /// Finished orientation attempts.
    attempts: Vec<IdentificationAttempt>,
    /// OCR prepared for the current visual attempt.
    pending: Option<PreparedAttempt>,
    /// Active processing time, excluding time between calls.
    active_ms: f64,
    /// Completed broad reference-alignment retries after both fast attempts failed.
    alignment_retry: usize,
}

impl CropIdentification {
    /// Prepare an operation without decoding, OCR or catalogue search.
    pub(crate) fn new(
        layout: FrameLayout,
        rotation: u16,
        corners: [[f32; 2]; 4],
        area: f32,
        options: &CandidateOptions,
    ) -> Self {
        Self {
            layout,
            rotation,
            corners,
            area,
            options: options.clone(),
            attempts: Vec::new(),
            pending: None,
            active_ms: 0.0,
            alignment_retry: 0,
        }
    }

    /// Resume actual matching, retaining raw OCR if the storage asks for reference pixels.
    ///
    /// # Errors
    ///
    /// Returns OCR/matching errors. Missing-reference requests preserve the current
    /// orientation's prepared reading so asynchronous callers never repeat OCR.
    pub(crate) fn advance(
        &mut self,
        engine: &MagicOcr,
        image: &DynamicImage,
        identify: &mut impl FnMut(
            &TextObservations,
            &CandidateOptions,
        ) -> Result<FieldIdentification, Error>,
    ) -> Result<IdentifiedCard, Error> {
        let started = Instant::now();
        let mut result = self.advance_inner(engine, image, identify);
        self.active_ms += started.elapsed().as_secs_f64() * 1000.0;
        if let Ok(card) = &mut result {
            card.timings.total_ms = self.active_ms;
        }
        result
    }

    /// Run until a final decision or the next unavailable selected reference image.
    ///
    /// # Errors
    ///
    /// Returns OCR or matching errors without discarding already completed readings.
    fn advance_inner(
        &mut self,
        engine: &MagicOcr,
        image: &DynamicImage,
        identify: &mut impl FnMut(
            &TextObservations,
            &CandidateOptions,
        ) -> Result<FieldIdentification, Error>,
    ) -> Result<IdentifiedCard, Error> {
        self.options.validate()?;
        while self.attempts.len() < 2 {
            let rotation = if self.attempts.is_empty() {
                self.rotation
            } else {
                (self.rotation + 180) % 360
            };
            if self.pending.is_none() {
                let started = Instant::now();
                let ocr = engine.read_title(image, self.layout, rotation)?;
                let title_ms = started.elapsed().as_secs_f64() * 1000.0;
                let started = Instant::now();
                let footer_ocr = engine.read_footer(image, self.layout, rotation)?;
                self.pending = Some(PreparedAttempt {
                    ocr,
                    footer_ocr,
                    title_ms,
                    footer_ms: started.elapsed().as_secs_f64() * 1000.0,
                    matching_ms: 0.0,
                });
            }
            let pending = self
                .pending
                .as_mut()
                .ok_or_else(|| Error::Backend("missing prepared OCR attempt".into()))?;
            let observations = TextObservations {
                title: pending.ocr.reading.text.clone(),
                footer: pending.footer_ocr.observations.footer.clone(),
                ..TextObservations::default()
            };
            let options = CandidateOptions {
                orientations: if rotation == 0 {
                    crate::matching::CandidateOrientations::Upright
                } else {
                    crate::matching::CandidateOrientations::Inverted
                },
                ..self.options.clone()
            };
            let started = Instant::now();
            let result = identify(&observations, &options);
            pending.matching_ms += started.elapsed().as_secs_f64() * 1000.0;
            let identification = result?;
            let pending = self
                .pending
                .take()
                .ok_or_else(|| Error::Backend("missing finished OCR attempt".into()))?;
            let accepted = accepted_candidate_identity(&identification.result);
            self.attempts.push(IdentificationAttempt {
                ocr: pending.ocr,
                footer_ocr: pending.footer_ocr,
                identification,
                restricted_reference_alignment: None,
                ocr_ms: pending.title_ms + pending.footer_ms,
                title_ocr_ms: pending.title_ms,
                footer_ocr_ms: pending.footer_ms,
                matching_ms: pending.matching_ms,
            });
            if accepted || self.attempts.len() == 2 {
                break;
            }
        }
        self.retry_reference_alignment(identify)?;
        let selected_attempt = select_identification_attempt(&self.attempts);
        let orientation_conflict = attempts_disagree(&self.attempts);
        let timings = IdentifyCardTimings {
            ocr_ms: self.attempts.iter().map(|a| a.ocr_ms).sum(),
            title_ocr_ms: self.attempts.iter().map(|a| a.title_ocr_ms).sum(),
            footer_ocr_ms: self.attempts.iter().map(|a| a.footer_ocr_ms).sum(),
            matching_ms: self.attempts.iter().map(|a| a.matching_ms).sum(),
            total_ms: 0.0,
        };
        Ok(IdentifiedCard {
            corners: self.corners,
            area: self.area,
            selected_attempt,
            orientation_conflict,
            attempts: std::mem::take(&mut self.attempts),
            timings,
        })
    }

    /// Reuse both failed OCR readings when reference storage orientation differs.
    ///
    /// A successful fast decision is preserved. Missing reference images retain
    /// the retry position and its cumulative time without running OCR again.
    fn retry_reference_alignment(
        &mut self,
        identify: &mut impl FnMut(
            &TextObservations,
            &CandidateOptions,
        ) -> Result<FieldIdentification, Error>,
    ) -> Result<(), Error> {
        if self.options.orientations != crate::matching::CandidateOrientations::UprightAndInverted
            || self
                .attempts
                .iter()
                .any(|attempt| accepted_candidate_identity(&attempt.identification.result))
        {
            return Ok(());
        }
        while self.alignment_retry < self.attempts.len() {
            let attempt = &mut self.attempts[self.alignment_retry];
            let observations = TextObservations {
                title: attempt.ocr.reading.text.clone(),
                footer: attempt.footer_ocr.observations.footer.clone(),
                ..TextObservations::default()
            };
            let started = Instant::now();
            let result = identify(&observations, &self.options);
            attempt.matching_ms += started.elapsed().as_secs_f64() * 1000.0;
            let broader = result?;
            let restricted = std::mem::replace(&mut attempt.identification, broader);
            attempt.restricted_reference_alignment = Some(restricted);
            self.alignment_retry += 1;
            if accepted_candidate_identity(&attempt.identification.result) {
                break;
            }
        }
        Ok(())
    }
}

/// Whether scoped geometry and observed text accepted a semantic identity.
fn accepted_candidate_identity(result: &mtg::identify::CandidateIdentification) -> bool {
    result.identity.card.is_some()
        && matches!(
            result.identity.status,
            mtg::identify::CombinedStatus::Matched
                | mtg::identify::CombinedStatus::VerifiedShortlist
        )
}

/// Choose the strongest attempted orientation without converting scores to confidence.
fn select_identification_attempt(attempts: &[IdentificationAttempt]) -> usize {
    let mut selected = 0;
    for candidate in 1..attempts.len() {
        if compare_identification_attempts(&attempts[candidate], &attempts[selected]).is_gt() {
            selected = candidate;
        }
    }
    selected
}

/// Order attempts by decision, actual verification count, then scoped retrieval scores.
fn compare_identification_attempts(
    left: &IdentificationAttempt,
    right: &IdentificationAttempt,
) -> std::cmp::Ordering {
    let left = &left.identification.result;
    let right = &right.identification.result;
    let rank = |status| match status {
        mtg::identify::CombinedStatus::Matched => 4_u8,
        mtg::identify::CombinedStatus::VerifiedShortlist => 3,
        mtg::identify::CombinedStatus::Conflict => 2,
        mtg::identify::CombinedStatus::Ambiguous => 1,
        mtg::identify::CombinedStatus::Unknown => 0,
    };
    rank(left.identity.status)
        .cmp(&rank(right.identity.status))
        .then_with(|| {
            left.visual
                .geometry_verified_count
                .cmp(&right.visual.geometry_verified_count)
        })
        .then_with(|| {
            let score = |attempt: &mtg::identify::CandidateIdentification| {
                attempt
                    .visual
                    .recognition
                    .candidates
                    .first()
                    .map_or(0.0, |candidate| candidate.similarity)
            };
            score(left).total_cmp(&score(right))
        })
        .then_with(|| {
            let score = |attempt: &mtg::identify::CandidateIdentification| {
                attempt
                    .text
                    .title
                    .first()
                    .and_then(|candidate| candidate.title_similarity)
                    .unwrap_or(0.0)
            };
            score(left).total_cmp(&score(right))
        })
}

/// Report contradictory strong title or collector identities across tried orientations.
fn attempts_disagree(attempts: &[IdentificationAttempt]) -> bool {
    attempts.iter().enumerate().any(|(offset, left)| {
        attempts[offset + 1..].iter().any(|right| {
            field_orientations_disagree(
                &left.identification.result.text,
                &right.identification.result.text,
            )
        })
    })
}

/// Include cross-channel disagreements without treating missing fields as contradictions.
fn field_orientations_disagree(
    left: &mtg::identify::TextCandidates,
    right: &mtg::identify::TextCandidates,
) -> bool {
    let left_footer = strong_footer_cards(&left.footer);
    let right_footer = strong_footer_cards(&right.footer);
    strong_title_sets_disagree(&left.title, &right.title)
        || disjoint_identity_sets(&left_footer, &right_footer)
        || disjoint_identity_sets(&strong_title_cards(&left.title), &right_footer)
        || disjoint_identity_sets(&left_footer, &strong_title_cards(&right.title))
}

/// Whether two near-best strong title sets have no semantic identity in common.
fn strong_title_sets_disagree(
    left: &[mtg::evidence::TextMatch],
    right: &[mtg::evidence::TextMatch],
) -> bool {
    let left = strong_title_cards(left);
    let right = strong_title_cards(right);
    disjoint_identity_sets(&left, &right)
}

/// Retain every language-compatible collector proposal for orientation-conflict reporting.
fn strong_footer_cards(matches: &[mtg::evidence::TextMatch]) -> Vec<&CardMetadata> {
    matches
        .iter()
        .filter(|hit| hit.footer_match && hit.language_match != Some(false))
        .map(|hit| &hit.card)
        .collect()
}

/// Whether two nonempty observations share no game-scoped semantic identity.
fn disjoint_identity_sets(left: &[&CardMetadata], right: &[&CardMetadata]) -> bool {
    !left.is_empty()
        && !right.is_empty()
        && !left
            .iter()
            .any(|left| right.iter().any(|right| same_card_identity(left, right)))
}

/// Return the near-best, strong title identities from one orientation attempt.
fn strong_title_cards(matches: &[mtg::evidence::TextMatch]) -> Vec<&CardMetadata> {
    let best = matches
        .iter()
        .filter_map(|candidate| candidate.title_similarity)
        .fold(0.0_f32, f32::max);
    matches
        .iter()
        .filter(|candidate| {
            candidate
                .title_similarity
                .is_some_and(|score| score >= 0.90 && score + 0.05 >= best)
        })
        .map(|candidate| &candidate.card)
        .collect()
}

/// Compare the game-scoped semantic grouping used by matching and fusion.
fn same_card_identity(left: &CardMetadata, right: &CardMetadata) -> bool {
    left.game == right.game
        && match (&left.oracle_id, &right.oracle_id) {
            (Some(left), Some(right)) => left == right,
            (None, None) => left.id == right.id,
            _ => false,
        }
}

/// Orientation conflict regressions run against the shared host-independent implementation.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::CandidateOrientations;

    /// Invent an abstention for state-machine tests without implying any image verification ran.
    fn alignment_result(reason: &str, accepted: bool) -> FieldIdentification {
        let mut result: FieldIdentification = serde_json::from_value(serde_json::json!({
            "observations": {"title": "", "footer": "", "rules": ""},
            "text": {"title": [], "rules": [], "footer": [], "hints": []},
            "visual": {
                "recognition": {
                    "status": "unknown", "identity_status": "unknown",
                    "confidence": 0.0, "identity_confidence": 0.0, "candidates": []
                },
                "candidate_printing_margin": 0.0, "candidate_identity_margin": 0.0,
                "scope": "supplied_candidates", "search_complete": false,
                "total_references": 2, "proposal_count": 0, "matched_proposals": 0,
                "candidate_count": 0, "descriptors_scored": 0, "geometry_attempted": 0,
                "geometry_candidates": [], "geometry_verified_count": 0,
                "budget_exhausted": false, "omitted_candidate_groups": 0,
                "descriptor_budget_exhausted": false, "geometry_budget_exhausted": false,
                "omitted_descriptor_groups": 0, "omitted_geometry_groups": 0,
                "geometry_verified": []
            },
            "identity": {
                "status": "unknown", "card": null, "scope": "supplied_candidates",
                "sources": [], "reason": reason
            },
            "printing": {
                "status": "unknown", "card": null, "scope": "supplied_candidates",
                "reason": "Invented state fixture; no printing decision."
            },
            "conflicts": []
        }))
        .expect("invented state fixture is valid");
        if accepted {
            result.result.identity.status = mtg::identify::CombinedStatus::VerifiedShortlist;
            result.result.identity.card = Some(mtg::identify::CardIdentity {
                game: "invented".into(),
                oracle_id: Some("alignment-identity".into()),
                printing_id: None,
                name: "Invented Observatory".into(),
            });
        }
        result
    }

    /// Preserve all public OCR fields while making the absence of inference explicit.
    fn retained_reading(text: &str, image_size: [u32; 2]) -> ocracle::Recognition {
        let line = ocracle::Line {
            text: text.into(),
            confidence: 0.0,
            bounds: [0, 0, image_size[0], image_size[1]],
        };
        ocracle::Recognition {
            text: text.into(),
            confidence: 0.0,
            confidence_kind: "Synthetic state fixture; no inference executed.".into(),
            method: "synthetic fixture".into(),
            image_size,
            lines: vec![line.clone()],
            candidates: vec![ocracle::Candidate {
                method: "synthetic fixture".into(),
                text: text.into(),
                confidence: 0.0,
                lines: vec![line],
            }],
            model: ocracle::ModelIdentity {
                backend: ocracle::InferenceBackend::Rten,
                name: "Invented recognition fixture".into(),
                language: "en".into(),
                sha256: "a".repeat(64),
                detector_sha256: None,
            },
        }
    }

    /// An already completed narrow pass with literal, explicitly oriented OCR evidence.
    fn retained_attempt(rotation: u16) -> IdentificationAttempt {
        let title = format!("Invented title at {rotation} degrees");
        let footer = format!("R0042 TST EN {rotation}");
        let observations = TextObservations {
            title: title.clone(),
            footer: footer.clone(),
            ..TextObservations::default()
        };
        let mut identification = alignment_result(&format!("narrow {rotation}"), false);
        identification.result.observations = observations.clone();
        IdentificationAttempt {
            ocr: mtg::ocr::TitleOcrEvidence {
                rotation_degrees: rotation,
                frame_layout: FrameLayout::Modern,
                oriented_image_size: [488, 680],
                crop_bounds: [30, 27, 360, 41],
                reading: retained_reading(&title, [360, 41]),
            },
            footer_ocr: mtg::ocr::FooterOcrEvidence {
                rotation_degrees: rotation,
                frame_layout: FrameLayout::Modern,
                oriented_image_size: [488, 680],
                crop_bounds: [21, 634, 77, 29],
                rows: vec![
                    mtg::ocr::FooterOcrRow {
                        crop_bounds: [21, 634, 70, 16],
                        input_bounds: [0, 0, 70, 16],
                    },
                    mtg::ocr::FooterOcrRow {
                        crop_bounds: [21, 646, 77, 17],
                        input_bounds: [78, 0, 77, 17],
                    },
                ],
                reading: retained_reading(&footer, [155, 17]),
                observations: TextObservations {
                    footer,
                    ..TextObservations::default()
                },
            },
            identification,
            restricted_reference_alignment: None,
            ocr_ms: 7.0,
            title_ocr_ms: 3.0,
            footer_ocr_ms: 4.0,
            matching_ms: 10.0,
        }
    }

    /// Seed completed fast passes so retry tests never construct an OCR engine or model.
    fn retained_work(orientations: CandidateOrientations) -> CropIdentification {
        let mut work = CropIdentification::new(
            FrameLayout::Modern,
            0,
            [[0.0, 0.0], [488.0, 0.0], [488.0, 680.0], [0.0, 680.0]],
            488.0 * 680.0,
            &CandidateOptions {
                max_descriptors: 7,
                max_geometry: 3,
                orientations,
            },
        );
        work.attempts = vec![retained_attempt(0), retained_attempt(180)];
        work
    }

    /// Snapshot raw crops, model identity and literal readings, independently of matching results.
    fn ocr_snapshot(attempts: &[IdentificationAttempt]) -> serde_json::Value {
        serde_json::to_value(
            attempts
                .iter()
                .map(|attempt| {
                    (
                        &attempt.ocr,
                        &attempt.footer_ocr,
                        attempt.ocr_ms,
                        attempt.title_ocr_ms,
                        attempt.footer_ocr_ms,
                    )
                })
                .collect::<Vec<_>>(),
        )
        .expect("synthetic OCR evidence serializes")
    }

    /// Either successful fast orientation bypasses all broader matching work.
    #[test]
    fn accepted_fast_alignment_does_not_run_reference_retry() {
        for accepted_position in 0..2 {
            let mut work = retained_work(CandidateOrientations::UprightAndInverted);
            work.attempts.truncate(accepted_position + 1);
            work.attempts[accepted_position].identification =
                alignment_result("fast accepted", true);
            let before = serde_json::to_value(&work.attempts).unwrap();
            work.retry_reference_alignment(&mut |_, _| {
                panic!("accepted fast evidence must not request broader alignment")
            })
            .unwrap();
            assert_eq!(work.alignment_retry, 0);
            assert_eq!(serde_json::to_value(&work.attempts).unwrap(), before);
        }
    }

    /// Broader retries retain narrow evidence and finish once, with or without an acceptance.
    #[test]
    fn broader_reference_alignment_preserves_narrow_provenance_and_literal_ocr() {
        for accepted_position in 0..=2 {
            let mut work = retained_work(CandidateOrientations::UprightAndInverted);
            let ocr_before = ocr_snapshot(&work.attempts);
            let narrow_before: Vec<_> = work
                .attempts
                .iter()
                .map(|attempt| serde_json::to_value(&attempt.identification).unwrap())
                .collect();
            let mut calls = 0;
            work.retry_reference_alignment(&mut |observations, options| {
                assert_eq!(options, &work_options());
                assert_eq!(
                    observations.title,
                    format!("Invented title at {} degrees", calls * 180)
                );
                let mut result = alignment_result("broader result", calls == accepted_position);
                result.result.observations = observations.clone();
                calls += 1;
                Ok(result)
            })
            .unwrap();
            let attempted = (accepted_position + 1).min(2);
            assert_eq!(calls, attempted);
            assert_eq!(work.alignment_retry, attempted);
            assert_eq!(ocr_snapshot(&work.attempts), ocr_before);
            for (position, attempt) in work.attempts.iter().enumerate() {
                if position < attempted {
                    assert_eq!(
                        serde_json::to_value(
                            attempt.restricted_reference_alignment.as_ref().unwrap()
                        )
                        .unwrap(),
                        narrow_before[position]
                    );
                    assert_eq!(
                        attempt.identification.result.identity.reason,
                        "broader result"
                    );
                } else {
                    assert!(attempt.restricted_reference_alignment.is_none());
                    assert_eq!(
                        serde_json::to_value(&attempt.identification).unwrap(),
                        narrow_before[position]
                    );
                }
            }
            work.retry_reference_alignment(&mut |_, _| {
                panic!("a completed broad retry must not be repeated")
            })
            .unwrap();
        }
    }

    /// Nondefault budgets must survive broad retries as well as their orientation policy.
    fn work_options() -> CandidateOptions {
        CandidateOptions {
            max_descriptors: 7,
            max_geometry: 3,
            orientations: CandidateOrientations::UprightAndInverted,
        }
    }

    /// Repeated sparse-reference suspensions resume only unfinished matching and retain its time.
    #[cfg(feature = "web")]
    #[test]
    fn missing_references_resume_alignment_without_repeating_completed_work_or_ocr() {
        let mut work = retained_work(CandidateOrientations::UprightAndInverted);
        let ocr_before = ocr_snapshot(&work.attempts);
        let narrow_before: Vec<_> = work
            .attempts
            .iter()
            .map(|attempt| serde_json::to_value(&attempt.identification).unwrap())
            .collect();
        let calls = std::cell::RefCell::new(Vec::new());
        let mut identify = |observations: &TextObservations, options: &CandidateOptions| {
            assert_eq!(options, &work_options());
            let position = calls.borrow().len();
            calls
                .borrow_mut()
                .push((observations.title.clone(), observations.footer.clone()));
            std::thread::sleep(std::time::Duration::from_millis(1));
            match position {
                0 => Ok(alignment_result("completed upright retry", false)),
                1 => Err(Error::MissingReferenceImages(vec![17])),
                2 => Err(Error::MissingReferenceImages(vec![18])),
                3 => Ok(alignment_result("completed inverted retry", true)),
                _ => panic!("completed reference-alignment retries must not run again"),
            }
        };
        assert!(matches!(
            work.retry_reference_alignment(&mut identify),
            Err(Error::MissingReferenceImages(indices)) if indices == [17]
        ));
        assert_eq!(work.alignment_retry, 1);
        let upright_time = work.attempts[0].matching_ms;
        let first_inverted_time = work.attempts[1].matching_ms;
        assert!(upright_time > 10.0);
        assert!(first_inverted_time > 10.0);
        assert!(work.attempts[0].restricted_reference_alignment.is_some());
        assert!(work.attempts[1].restricted_reference_alignment.is_none());
        assert_eq!(
            serde_json::to_value(&work.attempts[1].identification).unwrap(),
            narrow_before[1]
        );
        assert!(matches!(
            work.retry_reference_alignment(&mut identify),
            Err(Error::MissingReferenceImages(indices)) if indices == [18]
        ));
        assert_eq!(work.alignment_retry, 1);
        assert_eq!(work.attempts[0].matching_ms, upright_time);
        assert!(work.attempts[1].matching_ms > first_inverted_time);
        let second_inverted_time = work.attempts[1].matching_ms;
        assert_eq!(ocr_snapshot(&work.attempts), ocr_before);

        work.retry_reference_alignment(&mut identify).unwrap();
        assert_eq!(work.alignment_retry, 2);
        assert_eq!(work.attempts[0].matching_ms, upright_time);
        assert!(work.attempts[1].matching_ms > second_inverted_time);
        assert_eq!(ocr_snapshot(&work.attempts), ocr_before);
        for (position, attempt) in work.attempts.iter().enumerate() {
            assert_eq!(
                serde_json::to_value(attempt.restricted_reference_alignment.as_ref().unwrap())
                    .unwrap(),
                narrow_before[position]
            );
        }
        assert_eq!(calls.borrow().len(), 4);
        assert_eq!(
            calls.borrow()[0],
            (
                "Invented title at 0 degrees".into(),
                "R0042 TST EN 0".into()
            )
        );
        assert!(calls.borrow()[1..].iter().all(|(title, footer)| {
            title == "Invented title at 180 degrees" && footer == "R0042 TST EN 180"
        }));
        work.retry_reference_alignment(&mut identify).unwrap();
        assert_eq!(calls.borrow().len(), 4);
    }

    /// A caller restricting reference alignment never opts into the broader fallback.
    #[test]
    fn single_reference_orientation_does_not_broaden_failed_attempts() {
        for orientations in [
            CandidateOrientations::Upright,
            CandidateOrientations::Inverted,
        ] {
            let mut work = retained_work(orientations);
            let before = serde_json::to_value(&work.attempts).unwrap();
            work.retry_reference_alignment(&mut |_, _| {
                panic!("a single-orientation caller did not permit broader alignment")
            })
            .unwrap();
            assert_eq!(work.alignment_retry, 0);
            assert_eq!(serde_json::to_value(&work.attempts).unwrap(), before);
        }
    }

    /// Orientation conflict compares strong identity sets rather than first-result order.
    #[test]
    fn title_orientation_conflict_uses_near_best_identity_sets() {
        let card = |id: &str| CardMetadata {
            game: "invented".into(),
            id: id.into(),
            oracle_id: Some(id.into()),
            name: id.into(),
            ..CardMetadata::default()
        };
        let hit = |card: &CardMetadata, score| mtg::evidence::TextMatch {
            card: card.clone(),
            title_similarity: Some(score),
            rules_similarity: None,
            rules_coverage: None,
            footer_match: false,
            language_match: None,
            score,
            reasons: Vec::new(),
        };
        let first = card("first");
        let second = card("second");
        let other = card("other");
        assert!(!strong_title_sets_disagree(
            &[hit(&first, 0.95), hit(&second, 0.94)],
            &[hit(&second, 0.95), hit(&first, 0.94)]
        ));
        assert!(strong_title_sets_disagree(
            &[hit(&first, 1.0)],
            &[hit(&other, 0.96)]
        ));
        assert!(!strong_title_sets_disagree(
            &[hit(&first, 0.89)],
            &[hit(&other, 1.0)]
        ));
    }

    /// A readable title and an opposite footer conflict unless their identity sets overlap.
    #[test]
    fn opposite_footer_retains_identity_alternatives_and_language_disagreements() {
        let card = |id: &str| CardMetadata {
            game: "invented".into(),
            id: id.into(),
            oracle_id: Some(id.into()),
            ..CardMetadata::default()
        };
        let hit = |id: &str, footer_match: bool| mtg::evidence::TextMatch {
            card: card(id),
            title_similarity: (!footer_match).then_some(1.0),
            rules_similarity: None,
            rules_coverage: None,
            footer_match,
            language_match: None,
            score: 1.0,
            reasons: Vec::new(),
        };
        let candidates = |title, footer| mtg::identify::TextCandidates {
            title_identities: Vec::new(),
            title,
            rules: Vec::new(),
            footer,
            hints: Vec::new(),
        };
        let upright = candidates(vec![hit("first", false)], Vec::new());
        let mut inverted = candidates(Vec::new(), vec![hit("other", true)]);
        assert!(field_orientations_disagree(&upright, &inverted));
        inverted.footer.push(hit("first", true));
        assert!(!field_orientations_disagree(&upright, &inverted));
        inverted.footer.pop();
        inverted.footer[0].language_match = Some(false);
        assert!(!field_orientations_disagree(&upright, &inverted));
    }
}
