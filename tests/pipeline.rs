//! End-to-end extraction, recognition, and rejection on original procedural cards.

use whosthatpokemon::{
    CardMetadata, CatalogIndex, MatchOptions, MatchStatus, RecognitionPipeline,
    image::{DynamicImage, Rgb, RgbImage, imageops},
};

/// Draw a distinctive synthetic card without relying on downloaded artwork.
fn reference(seed: u32) -> DynamicImage {
    let mut image = RgbImage::from_pixel(180, 252, Rgb([238, 235, 225]));
    for y in 25..135 {
        for x in 16..164 {
            let wave = ((x * 7 + y * 3 + seed * 31) % 97) as u8;
            let color = if seed == 1 {
                [wave + 80, (x + y) as u8, 185 - wave]
            } else {
                [190 - wave, wave + 75, (x * 3 + y * 5) as u8]
            };
            image.put_pixel(x, y, Rgb(color));
        }
    }
    for line in 0..6 {
        for y in 160 + line * 9..163 + line * 9 {
            for x in 22..155 - line * 7 {
                image.put_pixel(x, y, Rgb([45, 48, 42]));
            }
        }
    }
    DynamicImage::ImageRgb8(image)
}

/// Enroll two unrelated identities using the public library API.
fn index() -> CatalogIndex {
    let mut index = CatalogIndex::new();
    for seed in 1..=2 {
        index
            .add(
                CardMetadata {
                    game: "test".into(),
                    id: format!("card-{seed}"),
                    name: format!("Test Card {seed}"),
                    oracle_id: Some(format!("identity-{seed}")),
                    ..CardMetadata::default()
                },
                &reference(seed),
            )
            .unwrap();
    }
    index
}

/// Scene scanning rectifies separated cards and associates their original corners.
#[test]
fn scans_multiple_cards_with_ranked_confidence() {
    let mut scene = RgbImage::from_pixel(640, 420, Rgb([24, 31, 37]));
    imageops::replace(&mut scene, &reference(1).to_rgb8(), 60, 70);
    let rotated = imageops::rotate180(&reference(2).to_rgb8());
    imageops::replace(&mut scene, &rotated, 365, 80);
    let report = RecognitionPipeline::new(index())
        .unwrap()
        .scan(&DynamicImage::ImageRgb8(scene))
        .unwrap();
    assert_eq!((report.width, report.height), (640, 420));
    assert_eq!(report.cards.len(), 2, "{report:?}");
    let identifiers = report
        .cards
        .iter()
        .map(|card| {
            assert_eq!(
                card.recognition.identity_status,
                MatchStatus::Matched,
                "{card:?}"
            );
            assert!(card.area > 40_000.0);
            assert!((0.0..=1.0).contains(&card.recognition.confidence));
            card.recognition.candidates[0].card.id.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(identifiers, ["card-1", "card-2"]);
    assert!(report.cards[0].corners.iter().all(|point| point[0] < 250.0));
    assert!(report.cards[1].corners.iter().all(|point| point[0] > 350.0));
}

/// No detected outline and an unrecognized direct crop have distinct results.
#[test]
fn no_detection_is_distinct_from_unknown_card() {
    let blank = DynamicImage::ImageRgb8(RgbImage::from_pixel(480, 420, Rgb([25, 30, 35])));
    let pipeline = RecognitionPipeline::new(index()).unwrap();
    assert!(pipeline.scan(&blank).unwrap().cards.is_empty());
    let unknown = pipeline.recognize_crop(&blank).unwrap();
    assert_eq!(unknown.status, MatchStatus::Unknown);
    assert_eq!(unknown.confidence, 0.0);
}

/// Direct-crop matching supports sideways inputs and preserves printing ambiguity.
#[test]
fn cropped_rotation_and_shared_art_do_not_invent_printing_certainty() {
    let mut index = index();
    index
        .add(
            CardMetadata {
                game: "test".into(),
                id: "reprint".into(),
                name: "Test Card 1".into(),
                oracle_id: Some("identity-1".into()),
                ..CardMetadata::default()
            },
            &reference(1),
        )
        .unwrap();
    let pipeline = RecognitionPipeline::new(index).unwrap();
    let rotated = DynamicImage::ImageRgb8(imageops::rotate90(&reference(1).to_rgb8()));
    let result = pipeline.recognize_crop(&rotated).unwrap();
    assert_eq!(result.status, MatchStatus::Ambiguous);
    assert_eq!(result.identity_status, MatchStatus::Matched);
    assert_eq!(result.confidence, 0.0);
    assert!(result.identity_confidence > result.confidence);
}

/// Invalid image/settings fail through the public API instead of producing scores.
#[test]
fn rejects_invalid_inputs_and_thresholds() {
    let pipeline = RecognitionPipeline::new(index()).unwrap();
    assert!(
        pipeline
            .recognize_crop(&DynamicImage::ImageRgb8(RgbImage::new(0, 0)))
            .is_err()
    );
    assert!(
        pipeline
            .scan(&DynamicImage::ImageRgb8(RgbImage::new(0, 0)))
            .is_err()
    );
    let options = MatchOptions {
        min_similarity: f32::NAN,
        ..MatchOptions::default()
    };
    assert!(RecognitionPipeline::with_config(index(), Default::default(), options).is_err());
}
