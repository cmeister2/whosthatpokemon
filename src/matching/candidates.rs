//! Storage adapters and sufficient ranking statistics shared by bounded recognition.

#[cfg(feature = "web")]
use std::collections::HashMap;

#[cfg(feature = "web")]
use super::CatalogIndex;
use super::{Reference, finish_recognition, local, same_face, same_identity};
#[cfg(feature = "web")]
use crate::MatchStatus;
use crate::{CardMetadata, Error, MatchCandidate, MatchOptions, Recognition};

/// Physical position of one face in the immutable sharded catalogue.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct CandidateLocation {
    /// Zero-based descriptor shard number.
    pub(super) shard: usize,
    /// Zero-based record offset inside the shard.
    pub(super) offset: usize,
}

/// Indexed expansion of one supplied OCR/catalogue proposal.
#[derive(Debug, Default)]
pub(super) struct CandidateMatches {
    /// The exact printing and physical face, when enrolled.
    pub(super) exact: Vec<CandidateLocation>,
    /// Every enrolled face with the same game-scoped semantic identity.
    pub(super) identity: Vec<CandidateLocation>,
    /// Every enrolled face with the same game-scoped illustration.
    pub(super) illustration: Vec<CandidateLocation>,
}

/// Storage operations required by the bounded candidate recognizer.
pub(super) trait CandidateSource {
    /// Return the full enrolled face count without searching it.
    fn len(&self) -> usize;
    /// Resolve direct printing, semantic identity and illustration postings.
    fn candidate_matches(&self, card: &CardMetadata) -> Result<CandidateMatches, Error>;
    /// Borrow the canonical enrolled metadata at a resolved location.
    fn candidate_metadata(&self, location: CandidateLocation) -> Result<&CardMetadata, Error>;
    /// Read only the requested global descriptor records.
    fn read_candidate_globals(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Reference>>, Error>;
    /// Read only the requested local feature records.
    fn read_candidate_local_features(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Vec<local::Feature>>>, Error>;
    /// Preserve shared-art ambiguity when the search happens to cover the gallery.
    fn illustration_ambiguity(&self, recognition: &mut Recognition) -> Result<(), Error>;
}

/// Startup-built direct metadata postings for a portable in-memory catalogue.
#[cfg(feature = "web")]
#[derive(Debug, Default)]
pub(crate) struct CatalogCandidateLookup {
    /// Canonical game and printing keys in enrollment order.
    printings: HashMap<(String, String), Vec<usize>>,
    /// Canonical game and Oracle keys in enrollment order.
    identities: HashMap<(String, String), Vec<usize>>,
    /// Canonical game and illustration keys in enrollment order.
    illustrations: HashMap<(String, String), Vec<usize>>,
}

#[cfg(feature = "web")]
impl CatalogCandidateLookup {
    /// Build direct lookup once while the catalogue is initialized.
    pub(crate) fn new(index: &CatalogIndex) -> Self {
        let mut lookup = Self::default();
        for (offset, card) in index.metadata().enumerate() {
            lookup
                .printings
                .entry((card.game.clone(), card.id.clone()))
                .or_default()
                .push(offset);
            if let Some(identity) = &card.oracle_id {
                lookup
                    .identities
                    .entry((card.game.clone(), identity.clone()))
                    .or_default()
                    .push(offset);
            }
            if let Some(illustration) = &card.illustration_id {
                lookup
                    .illustrations
                    .entry((card.game.clone(), illustration.clone()))
                    .or_default()
                    .push(offset);
            }
        }
        lookup
    }

    /// Verify only the direct indexed closure of the supplied metadata hints.
    pub(crate) fn recognize(
        &self,
        index: &CatalogIndex,
        image: &image::DynamicImage,
        options: &MatchOptions,
        candidate_options: &super::CandidateOptions,
        candidates: &[CardMetadata],
    ) -> Result<super::CandidateRecognition, Error> {
        super::shortlist::recognize_candidates_with(
            &self.source(index),
            image,
            options,
            candidate_options,
            candidates,
        )
    }

    /// Bind immutable reference data to the matching startup-built postings.
    pub(crate) fn source<'a>(&'a self, index: &'a CatalogIndex) -> CatalogCandidateSource<'a> {
        CatalogCandidateSource {
            index,
            lookup: self,
        }
    }
}

/// One immutable in-memory catalogue together with its matching direct postings.
#[cfg(feature = "web")]
pub(crate) struct CatalogCandidateSource<'a> {
    /// Validated descriptors and local features in original enrollment order.
    index: &'a CatalogIndex,
    /// Direct metadata keys built from this same immutable catalogue.
    lookup: &'a CatalogCandidateLookup,
}

#[cfg(feature = "web")]
impl CatalogCandidateSource<'_> {
    /// Borrow a selected face, rejecting impossible adapter locations.
    fn reference(&self, shard: usize, offset: usize) -> Result<&Reference, Error> {
        if shard != 0 {
            return Err(Error::InvalidIndex(
                "invalid portable candidate shard".into(),
            ));
        }
        self.index
            .references
            .get(offset)
            .ok_or_else(|| Error::InvalidIndex("invalid portable candidate offset".into()))
    }
}

#[cfg(feature = "web")]
impl CandidateSource for CatalogCandidateSource<'_> {
    fn len(&self) -> usize {
        self.index.len()
    }

    fn candidate_matches(&self, card: &CardMetadata) -> Result<CandidateMatches, Error> {
        let printing = self
            .lookup
            .printings
            .get(&(card.game.clone(), card.id.clone()))
            .map_or(&[][..], Vec::as_slice);
        let locations = |offsets: &[usize]| {
            offsets
                .iter()
                .map(|&offset| CandidateLocation { shard: 0, offset })
                .collect::<Vec<_>>()
        };
        let exact = printing
            .iter()
            .copied()
            .filter(|&offset| self.index.references[offset].card.face_index == card.face_index)
            .collect::<Vec<_>>();
        let identity = if let Some(identity) = &card.oracle_id {
            self.lookup
                .identities
                .get(&(card.game.clone(), identity.clone()))
                .cloned()
                .unwrap_or_default()
        } else {
            printing
                .iter()
                .copied()
                .filter(|&offset| self.index.references[offset].card.oracle_id.is_none())
                .collect()
        };
        let illustration = card
            .illustration_id
            .as_ref()
            .and_then(|illustration| {
                self.lookup
                    .illustrations
                    .get(&(card.game.clone(), illustration.clone()))
            })
            .map_or(&[][..], Vec::as_slice);
        Ok(CandidateMatches {
            exact: locations(&exact),
            identity: locations(&identity),
            illustration: locations(illustration),
        })
    }

    fn candidate_metadata(&self, location: CandidateLocation) -> Result<&CardMetadata, Error> {
        Ok(&self.reference(location.shard, location.offset)?.card)
    }

    fn read_candidate_globals(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Reference>>, Error> {
        let mut selected = vec![None; offsets.iter().max().map_or(0, |offset| offset + 1)];
        for &offset in offsets {
            let reference = self.reference(shard, offset)?;
            selected[offset] = Some(Reference {
                card: reference.card.clone(),
                views: reference.views.clone(),
                features: Vec::new(),
            });
        }
        Ok(selected)
    }

    fn read_candidate_local_features(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Vec<local::Feature>>>, Error> {
        let mut selected = vec![None; offsets.iter().max().map_or(0, |offset| offset + 1)];
        for &offset in offsets {
            selected[offset] = Some(self.reference(shard, offset)?.features.clone());
        }
        Ok(selected)
    }

    fn illustration_ambiguity(&self, result: &mut Recognition) -> Result<(), Error> {
        let Some(best) = result.candidates.first() else {
            return Ok(());
        };
        let Some(illustration) = &best.card.illustration_id else {
            return Ok(());
        };
        let offsets = self
            .lookup
            .illustrations
            .get(&(best.card.game.clone(), illustration.clone()))
            .map_or(&[][..], Vec::as_slice);
        let mut printing_ambiguous = false;
        let mut identity_ambiguous = false;
        for &offset in offsets {
            let card = &self.index.references[offset].card;
            if !same_face(card, &best.card) {
                printing_ambiguous = true;
                identity_ambiguous |= !same_identity(card, &best.card);
            }
        }
        if printing_ambiguous {
            result.status = MatchStatus::Ambiguous;
            result.confidence = 0.0;
            for candidate in &mut result.candidates {
                candidate.confidence = 0.0;
            }
        }
        if identity_ambiguous {
            result.identity_status = MatchStatus::Ambiguous;
            result.identity_confidence = 0.0;
        }
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl CandidateSource for super::ShardedIndex {
    fn len(&self) -> usize {
        self.len()
    }
    fn candidate_matches(&self, card: &CardMetadata) -> Result<CandidateMatches, Error> {
        self.candidate_matches(card)
    }
    fn candidate_metadata(&self, location: CandidateLocation) -> Result<&CardMetadata, Error> {
        self.candidate_metadata(location)
    }
    fn read_candidate_globals(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Reference>>, Error> {
        self.read_candidate_globals(shard, offsets)
    }
    fn read_candidate_local_features(
        &self,
        shard: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Vec<local::Feature>>>, Error> {
        self.read_candidate_local_features(shard, offsets)
    }
    fn illustration_ambiguity(&self, recognition: &mut Recognition) -> Result<(), Error> {
        self.illustration_ambiguity(recognition)
    }
}

/// Sufficient full-gallery statistics for exact top-k and both runner-up margins.
pub(super) struct Ranking {
    /// Number of printing candidates retained, with at least the printing runner-up.
    limit: usize,
    /// Best printing candidates, with stable enrollment order for equal scores.
    pub(super) top: Vec<MatchCandidate>,
    /// Best representatives of the strongest two distinct semantic identities.
    pub(super) identities: Vec<MatchCandidate>,
}

impl Ranking {
    /// Reserve only the requested result count and constant-size margin evidence.
    pub(super) fn new(top_k: usize) -> Self {
        Self {
            limit: top_k.max(2),
            top: Vec::new(),
            identities: Vec::new(),
        }
    }

    /// Return exact printing and identity margins over all evidence consumed so far.
    pub(super) fn margins(&self) -> (f32, f32) {
        let Some(best) = self.top.first() else {
            return (0.0, 0.0);
        };
        let printing = self.top.get(1).map_or(0.0, |next| next.similarity);
        let identity = self
            .identities
            .iter()
            .find(|candidate| !same_identity(&best.card, &candidate.card))
            .map_or(0.0, |candidate| candidate.similarity);
        (
            (best.similarity - printing).max(0.0),
            (best.similarity - identity).max(0.0),
        )
    }

    /// Consume one candidate without discarding a possible identity runner-up.
    pub(super) fn push(&mut self, candidate: MatchCandidate) {
        if let Some(position) = self
            .identities
            .iter()
            .position(|known| same_identity(&known.card, &candidate.card))
        {
            if candidate.similarity > self.identities[position].similarity {
                self.identities[position] = candidate.clone();
                self.identities
                    .sort_by(|a, b| b.similarity.total_cmp(&a.similarity));
            }
        } else {
            let position = self
                .identities
                .iter()
                .position(|known| candidate.similarity > known.similarity)
                .unwrap_or(self.identities.len());
            if position < 2 {
                self.identities.insert(position, candidate.clone());
                self.identities.truncate(2);
            }
        }
        let position = self
            .top
            .iter()
            .position(|known| candidate.similarity > known.similarity)
            .unwrap_or(self.top.len());
        if position < self.limit {
            self.top.insert(position, candidate);
            self.top.truncate(self.limit);
        }
    }

    /// Compute the existing decisions using exact retained sufficient statistics.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid options or an empty catalogue.
    pub(super) fn finish(mut self, options: &MatchOptions) -> Result<Recognition, Error> {
        for candidate in self.identities {
            if !self
                .top
                .iter()
                .any(|known| same_face(&known.card, &candidate.card))
            {
                self.top.push(candidate);
            }
        }
        finish_recognition(self.top, options)
    }
}
