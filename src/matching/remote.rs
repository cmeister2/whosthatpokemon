//! Complete metadata postings with a bounded cache of actually acquired reference pixels.

use image::DynamicImage;
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
};

use super::{
    CameraProjection, CandidateOptions, CandidateRecognition, CatalogCandidateLookup, CatalogIndex,
    Reference,
    candidates::{CandidateLocation, CandidateMatches, CandidateSource},
    local,
};
use crate::{CardMetadata, Error, MatchOptions, Recognition};

/// Maximum resident full reference banks, independent of total catalogue size.
const CACHE_FACES: usize = 512;

/// Canonical physical-face metadata and the explicit source of its reference pixels.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteReference {
    /// Existing public card metadata, including exact title and printing identity.
    #[serde(flatten)]
    pub(crate) card: CardMetadata,
    /// Public canonical image URL supplied by the catalogue snapshot.
    pub(crate) image_url: String,
    /// Optional previously acquired image digest; absent for fresh bulk metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) image_sha256: Option<String>,
}

/// Image acquisition metadata aligned with the sole canonical card-metadata table.
struct ImageSource {
    /// Canonical public image URL.
    url: String,
    /// Optional snapshot digest for a previously acquired canonical image.
    sha256: Option<String>,
}

/// All physical metadata records with sparse, bounded visual reference storage.
pub(crate) struct RemoteCatalog {
    /// Canonical image sources and retained card metadata in catalogue order.
    sources: Vec<ImageSource>,
    /// Empty visual banks for unacquired records; real banks only for cached faces.
    index: CatalogIndex,
    /// Direct metadata postings built once from every enrolled physical face.
    lookup: CatalogCandidateLookup,
    /// Resident banks ordered from least to most recently required.
    lru: RefCell<VecDeque<usize>>,
    /// Faces required by the current bounded visual pass, protected during acquisition.
    pinned: RefCell<BTreeSet<usize>>,
}

impl RemoteCatalog {
    /// Validate the full metadata snapshot without enrolling or scoring any images.
    ///
    /// # Errors
    ///
    /// Returns an error for empty/duplicate metadata, invalid identities, image URLs or digests.
    pub(crate) fn new(records: Vec<RemoteReference>) -> Result<Self, Error> {
        if records.is_empty() {
            return Err(Error::InvalidCatalog(
                "empty remote reference catalogue".into(),
            ));
        }
        let mut seen = HashSet::new();
        let mut sources = Vec::with_capacity(records.len());
        let mut references = Vec::with_capacity(records.len());
        for record in records {
            super::validate_metadata(&record.card).map_err(Error::InvalidCatalog)?;
            if !record.image_url.starts_with("https://cards.scryfall.io/")
                || record.image_sha256.as_ref().is_some_and(|hash| {
                    hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit())
                })
            {
                return Err(Error::InvalidCatalog(
                    "invalid canonical image source or SHA-256".into(),
                ));
            }
            if !seen.insert((
                record.card.game.clone(),
                record.card.id.clone(),
                record.card.face_index,
            )) {
                return Err(Error::InvalidCatalog(
                    "duplicate remote physical face".into(),
                ));
            }
            sources.push(ImageSource {
                url: record.image_url,
                sha256: record.image_sha256,
            });
            references.push(Reference {
                card: record.card,
                views: Vec::new(),
                features: Vec::new(),
            });
        }
        let index = CatalogIndex { references };
        let lookup = CatalogCandidateLookup::new(&index);
        Ok(Self {
            sources,
            index,
            lookup,
            lru: RefCell::new(VecDeque::new()),
            pinned: RefCell::new(BTreeSet::new()),
        })
    }

    /// Iterate all metadata once for compact OCR lookup initialization.
    pub(crate) fn metadata(&self) -> impl Iterator<Item = &CardMetadata> {
        self.index.metadata()
    }

    /// Resolve exact physical-face identity without traversing the catalogue.
    ///
    /// # Errors
    ///
    /// Returns an error for a face outside the immutable metadata snapshot.
    pub(crate) fn position(&self, game: &str, id: &str, face: u32) -> Result<usize, Error> {
        let card = CardMetadata {
            game: game.to_owned(),
            id: id.to_owned(),
            face_index: face,
            ..CardMetadata::default()
        };
        self.lookup
            .source(&self.index)
            .candidate_matches(&card)?
            .exact
            .first()
            .map(|location| location.offset)
            .ok_or_else(|| Error::InvalidCatalog("reference face is outside this catalogue".into()))
    }

    /// Borrow one known image source without scanning metadata.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid storage position.
    pub(crate) fn record(&self, position: usize) -> Result<RemoteReference, Error> {
        let source = self
            .sources
            .get(position)
            .ok_or_else(|| Error::InvalidCatalog("unknown reference position".into()))?;
        let reference = self
            .index
            .references
            .get(position)
            .ok_or_else(|| Error::InvalidCatalog("unknown reference metadata".into()))?;
        Ok(RemoteReference {
            card: reference.card.clone(),
            image_url: source.url.clone(),
            image_sha256: source.sha256.clone(),
        })
    }

    /// Enroll actual canonical pixels through the unchanged native reference encoder.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsolicited face, malformed reference or a saturated protected cache.
    pub(crate) fn enroll(&mut self, position: usize, image: &DynamicImage) -> Result<(), Error> {
        if !self.pinned.borrow().contains(&position) {
            return Err(Error::InvalidInput(
                "reference pixels were not requested by this visual pass".into(),
            ));
        }
        let card = self.record(position)?.card.clone();
        // A one-face temporary index preserves native validation/encoding without
        // CatalogIndex::add's duplicate traversal over the full metadata table.
        let mut encoded = CatalogIndex::new();
        encoded.add(card, image)?;
        let reference = encoded
            .references
            .pop()
            .ok_or_else(|| Error::InvalidIndex("reference encoder returned no face".into()))?;
        let mut lru = self.lru.borrow_mut();
        lru.retain(|&loaded| loaded != position);
        while lru.len() >= CACHE_FACES {
            let evict = lru
                .iter()
                .position(|loaded| !self.pinned.borrow().contains(loaded))
                .ok_or_else(|| {
                    Error::InvalidInput("selected reference images exceed the cache budget".into())
                })?;
            let loaded = lru
                .remove(evict)
                .ok_or_else(|| Error::InvalidIndex("missing resident reference".into()))?;
            self.index.references[loaded].views = Vec::new();
            self.index.references[loaded].features = Vec::new();
        }
        self.index.references[position] = reference;
        lru.push_back(position);
        Ok(())
    }

    /// Invoke the existing bounded recognizer over full metadata and only loaded banks.
    ///
    /// # Errors
    ///
    /// Returns a resumable request when selected reference pixels are absent, or matching errors.
    pub(crate) fn recognize(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
        budget: &CandidateOptions,
        cards: &[CardMetadata],
        exact: bool,
    ) -> Result<CandidateRecognition, Error> {
        self.pinned.borrow_mut().clear();
        let source = SparseSource { catalog: self };
        if exact {
            super::shortlist::recognize_faces_with(&source, image, options, budget, cards)
        } else {
            super::shortlist::recognize_candidates_with(&source, image, options, budget, cards)
        }
    }

    /// Recompute camera-only local geometry for exact faces already verified for this same crop.
    ///
    /// Missing or featureless resident banks are skipped without requesting images,
    /// changing cache order or changing global/candidate recognition decisions.
    ///
    /// # Errors
    ///
    /// Returns invalid input for duplicate/excessive requests or invalid pixels,
    /// or an invalid-catalogue error for an unknown exact physical face.
    pub(crate) fn camera_projections(
        &self,
        image: &DynamicImage,
        cards: &[CardMetadata],
    ) -> Result<Vec<CameraProjection>, Error> {
        super::camera_projection::validate_requests(cards)?;
        let mut references = Vec::with_capacity(cards.len());
        for card in cards {
            let position = self.position(&card.game, &card.id, card.face_index)?;
            let reference = &self.index.references[position];
            if !reference.views.is_empty() && !reference.features.is_empty() {
                references.push(reference);
            }
        }
        super::camera_projection::project_resident(image, &references)
    }
}

/// Candidate-source adapter preserving complete metadata scope while requesting only absent banks.
struct SparseSource<'a> {
    /// Immutable snapshot and bounded reference cache.
    catalog: &'a RemoteCatalog,
}

impl CandidateSource for SparseSource<'_> {
    fn len(&self) -> usize {
        self.catalog.sources.len()
    }
    fn candidate_matches(&self, card: &CardMetadata) -> Result<CandidateMatches, Error> {
        self.catalog
            .lookup
            .source(&self.catalog.index)
            .candidate_matches(card)
    }
    fn candidate_metadata(&self, location: CandidateLocation) -> Result<&CardMetadata, Error> {
        if location.shard != 0 {
            return Err(Error::InvalidIndex("invalid sparse metadata shard".into()));
        }
        self.catalog
            .index
            .references
            .get(location.offset)
            .map(|reference| &reference.card)
            .ok_or_else(|| Error::InvalidIndex("unknown sparse metadata position".into()))
    }
    fn read_candidate_globals(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<BTreeMap<usize, Reference>, Error> {
        if shard != 0 {
            return Err(Error::InvalidIndex("invalid sparse candidate shard".into()));
        }
        self.catalog
            .pinned
            .borrow_mut()
            .extend(offsets.iter().copied());
        let selected: BTreeSet<_> = offsets.iter().copied().collect();
        let mut lru = self.catalog.lru.borrow_mut();
        lru.retain(|loaded| !selected.contains(loaded));
        for &offset in offsets {
            if self
                .catalog
                .index
                .references
                .get(offset)
                .is_some_and(|reference| !reference.views.is_empty())
            {
                lru.push_back(offset);
            }
        }
        drop(lru);
        let missing = offsets
            .iter()
            .copied()
            .filter(|&offset| {
                self.catalog
                    .index
                    .references
                    .get(offset)
                    .is_none_or(|reference| reference.views.is_empty())
            })
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(Error::MissingReferenceImages(missing));
        }
        self.catalog
            .lookup
            .source(&self.catalog.index)
            .read_candidate_globals(shard, offsets)
    }
    fn read_candidate_local_features(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<BTreeMap<usize, Vec<local::Feature>>, Error> {
        self.catalog
            .lookup
            .source(&self.catalog.index)
            .read_candidate_local_features(shard, offsets)
    }
    fn illustration_ambiguity(&self, result: &mut Recognition) -> Result<(), Error> {
        self.catalog
            .lookup
            .source(&self.catalog.index)
            .illustration_ambiguity(result)
    }
}

/// Sparse scope, late-catalogue selection and bounded eviction regressions.
#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    /// Invented portrait with enough independent structure for real descriptor enrollment.
    fn pixels() -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(120, 168, |x, y| {
            let value = x.wrapping_mul(37) ^ y.wrapping_mul(71) ^ (x * y / 13);
            Rgb([value as u8, (value >> 3) as u8, (value >> 7) as u8])
        }))
    }

    /// Distinct physical identities with only explicit public canonical URL metadata.
    fn records(count: usize) -> Vec<RemoteReference> {
        (0..count)
            .map(|n| RemoteReference {
                card: CardMetadata {
                    game: "invented".into(),
                    id: format!("face-{n}"),
                    name: format!("Face {n}"),
                    oracle_id: Some(format!("identity-{n}")),
                    ..CardMetadata::default()
                },
                image_url: format!("https://cards.scryfall.io/normal/front/test-{n}.jpg"),
                image_sha256: None,
            })
            .collect()
    }

    /// One face at the end of a large metadata snapshot still reads a one-entry sparse bank.
    #[test]
    fn late_face_requests_only_selected_pixels_and_preserves_full_scope() {
        let mut catalog = RemoteCatalog::new(records(10_000)).unwrap();
        let card = catalog.index.references[9_999].card.clone();
        let image = pixels();
        let options = MatchOptions::default();
        let budget = CandidateOptions::default();
        assert!(
            matches!(catalog.recognize(&image, &options, &budget, std::slice::from_ref(&card), true),
            Err(Error::MissingReferenceImages(faces)) if faces == [9_999])
        );
        catalog.enroll(9_999, &image).unwrap();
        let source = SparseSource { catalog: &catalog };
        let loaded = source.read_candidate_globals(0, &[9_999]).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&9_999));
        let result = catalog
            .recognize(&image, &options, &budget, &[card], true)
            .unwrap();
        assert_eq!(result.descriptors_scored, 1);
        assert_eq!(result.geometry_attempted, 1);
        assert!(!result.search_complete);
        assert_eq!(
            result.scope,
            super::super::ShortlistScope::SuppliedCandidates
        );
        assert!(
            result
                .recognition
                .candidates
                .iter()
                .all(|candidate| candidate.confidence == 0.0)
        );
    }

    /// Eviction releases feature allocations and never removes the currently selected bank.
    #[test]
    fn cache_evicts_unselected_faces_and_releases_owned_banks() {
        let mut catalog = RemoteCatalog::new(records(CACHE_FACES + 1)).unwrap();
        let image = pixels();
        let mut temporary = CatalogIndex::new();
        temporary
            .add(catalog.index.references[0].card.clone(), &image)
            .unwrap();
        let encoded = temporary.references.pop().unwrap();
        for position in 0..CACHE_FACES {
            catalog.index.references[position].views = encoded.views.clone();
            catalog.index.references[position].features = encoded.features.clone();
            catalog.lru.borrow_mut().push_back(position);
        }
        assert!(catalog.index.references[0].features.capacity() > 0);
        catalog.pinned.borrow_mut().extend([0, CACHE_FACES]);
        catalog.enroll(CACHE_FACES, &image).unwrap();
        assert_eq!(catalog.lru.borrow().len(), CACHE_FACES);
        assert!(!catalog.index.references[0].views.is_empty());
        assert_eq!(catalog.index.references[1].views.capacity(), 0);
        assert_eq!(catalog.index.references[1].features.capacity(), 0);
        assert!(!catalog.index.references[CACHE_FACES].views.is_empty());
    }

    /// Incoming pixels must correspond to a selected immutable physical face.
    #[test]
    fn rejects_unsolicited_reference_pixels_and_ambiguous_snapshot_keys() {
        let mut catalog = RemoteCatalog::new(records(2)).unwrap();
        assert!(catalog.enroll(0, &pixels()).is_err());
        let mut duplicate = records(2);
        duplicate[1].card = duplicate[0].card.clone();
        assert!(RemoteCatalog::new(duplicate).is_err());
        let mut untrusted = records(1);
        untrusted[0].image_url = "https://unrelated.invalid/image.jpg".into();
        assert!(RemoteCatalog::new(untrusted).is_err());
    }

    /// Camera diagnostics never fetch or pin an unacquired canonical reference bank.
    #[test]
    fn camera_projections_skip_missing_banks_without_cache_side_effects() {
        let catalog = RemoteCatalog::new(records(8)).unwrap();
        let cards = catalog.metadata().cloned().collect::<Vec<_>>();
        assert!(
            catalog
                .camera_projections(&pixels(), &cards)
                .unwrap()
                .is_empty()
        );
        assert!(catalog.lru.borrow().is_empty());
        assert!(catalog.pinned.borrow().is_empty());
        assert!(
            catalog
                .index
                .references
                .iter()
                .all(|reference| { reference.views.is_empty() && reference.features.is_empty() })
        );
        let empty = DynamicImage::ImageRgb8(RgbImage::new(0, 0));
        assert!(matches!(
            catalog.camera_projections(&empty, &cards),
            Err(Error::InvalidInput(_))
        ));
    }

    /// Request limits and exact physical identities are checked without image acquisition.
    #[test]
    fn camera_projections_bound_requests_and_reject_duplicate_or_unknown_faces() {
        let catalog = RemoteCatalog::new(records(9)).unwrap();
        let cards = catalog.metadata().cloned().collect::<Vec<_>>();
        assert!(matches!(
            catalog.camera_projections(&pixels(), &cards),
            Err(Error::InvalidInput(message)) if message.contains("eight-face")
        ));
        let duplicate = vec![cards[0].clone(), cards[0].clone()];
        assert!(matches!(
            catalog.camera_projections(&pixels(), &duplicate),
            Err(Error::InvalidInput(message)) if message.contains("duplicate")
        ));
        let mut unknown = cards[0].clone();
        unknown.id = "not-enrolled".into();
        assert!(matches!(
            catalog.camera_projections(&pixels(), &[unknown]),
            Err(Error::InvalidCatalog(_))
        ));
        assert!(catalog.pinned.borrow().is_empty());
        assert!(catalog.lru.borrow().is_empty());
    }

    /// Resident diagnostics preserve the exact existing query normalization and local evidence.
    #[test]
    fn camera_projections_return_unchanged_local_evidence_for_same_query() {
        let mut catalog = RemoteCatalog::new(records(1)).unwrap();
        let image = pixels();
        catalog.pinned.borrow_mut().insert(0);
        catalog.enroll(0, &image).unwrap();
        let mut requested = catalog.index.references[0].card.clone();
        requested.name = "caller metadata must not replace canonical bank metadata".into();
        let pinned = catalog.pinned.borrow().clone();
        let lru = catalog.lru.borrow().clone();
        for query in [image.clone(), DynamicImage::ImageRgba8(image.to_rgba8())] {
            let rgb = query.to_rgb8();
            let normalized = image::imageops::resize(
                &rgb,
                local::WIDTH,
                local::HEIGHT,
                image::imageops::FilterType::Triangle,
            );
            let expected = local::match_evidence(
                &local::extract_query(&query, &normalized),
                &catalog.index.references[0].features,
            )
            .unwrap();
            let actual = catalog
                .camera_projections(&query, std::slice::from_ref(&requested))
                .unwrap();
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].card, catalog.index.references[0].card);
            assert_eq!(actual[0].similarity, expected.score);
            assert_eq!(actual[0].rotation_degrees, expected.rotation_degrees);
            assert_eq!(actual[0].projection, expected.projection);
            assert!(actual[0].projection.iter().all(|value| value.is_finite()));
        }
        assert_eq!(*catalog.pinned.borrow(), pinned);
        assert_eq!(*catalog.lru.borrow(), lru);
        assert_eq!(
            super::super::CAMERA_FEATURE_DIMENSIONS,
            (local::WIDTH, local::HEIGHT)
        );
    }
}
