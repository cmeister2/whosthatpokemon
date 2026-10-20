//! Exact native retrieval with one validated descriptor shard resident at a time.

use std::{
    cmp::Ordering,
    collections::HashSet,
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use image::{DynamicImage, imageops::FilterType};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::finish_recognition;
use super::{
    CatalogIndex, Reference, SCHEME, binary,
    candidates::{CandidateLocation, CandidateMatches, Ranking},
    has_texture, local, query_descriptors, same_face, same_identity, score_reference,
    validate_image, validate_metadata, verify_geometry,
};
#[cfg(test)]
use crate::MatchCandidate;
use crate::{CardMetadata, Error, MatchOptions, MatchStatus, Matcher, Recognition};

/// Version of the manifest, independent of each shard's descriptor format.
const MANIFEST_VERSION: u32 = 2;
/// Final manifest name; a directory without it is an unfinished build.
const MANIFEST_FILE: &str = "manifest.json";
/// Recoverable manifest containing only completed shards.
const CHECKPOINT_FILE: &str = "checkpoint.json";

/// Persistence implementation; old manifests omit this field and remain JSON.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Storage {
    /// Existing version-one JSON descriptor shards.
    #[default]
    Json,
    /// Exact global vectors and local features in separate native binary files.
    SplitBinary,
}

/// One bounded shard, in exact enrollment order.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Shard {
    /// Relative filename, constrained to the generated sequence.
    file: String,
    /// Number of physical faces in this shard.
    references: usize,
}

/// Publication record written only after every shard has been saved.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    /// Manifest compatibility version.
    format_version: u32,
    /// Explicit backend discriminator.
    backend: String,
    /// Encoding used for the evidence files.
    #[serde(default)]
    storage: Storage,
    /// Descriptor/scoring compatibility contract for the native representation.
    #[serde(default)]
    scheme: Option<String>,
    /// Total physical-face count, including separate backs.
    references: usize,
    /// Maximum physical-face count per descriptor shard.
    shard_size: usize,
    /// Shards in original enrollment order, preserving deterministic ties.
    shards: Vec<Shard>,
}

/// Long-lived metadata postings used to avoid a catalogue scan for every query.
#[derive(Debug, Default)]
struct CandidateLookup {
    /// Enrollment offsets sorted by game, printing identifier and original order.
    printings: Vec<usize>,
    /// Enrollment offsets sorted by game, semantic identity and original order.
    identities: Vec<usize>,
    /// Enrollment offsets sorted by game, illustration identifier and original order.
    illustrations: Vec<usize>,
    /// Exclusive flat-card boundaries for every metadata shard.
    shard_ends: Vec<usize>,
    /// Shard number for each flat enrollment offset, avoiding nested binary searches.
    ordinal_shards: Vec<usize>,
}

impl CandidateLookup {
    /// Build stable enrollment-ordered postings without retaining duplicate string keys.
    fn new(metadata: &[Vec<CardMetadata>]) -> Result<Self, String> {
        let cards = metadata.iter().flatten().collect::<Vec<_>>();
        let mut printings = (0..cards.len()).collect::<Vec<_>>();
        let mut identities = (0..cards.len())
            .filter(|&ordinal| cards[ordinal].oracle_id.is_some())
            .collect::<Vec<_>>();
        let mut illustrations = (0..cards.len())
            .filter(|&ordinal| cards[ordinal].illustration_id.is_some())
            .collect::<Vec<_>>();

        // Group exact faces first so duplicates are adjacent. Re-sort afterward
        // to retain the former postings' enrollment order within each printing.
        printings.sort_unstable_by(|&left, &right| {
            let left = cards[left];
            let right = cards[right];
            left.game
                .cmp(&right.game)
                .then(left.id.cmp(&right.id))
                .then(left.face_index.cmp(&right.face_index))
        });
        if printings.windows(2).any(|pair| {
            let left = cards[pair[0]];
            let right = cards[pair[1]];
            left.game == right.game && left.id == right.id && left.face_index == right.face_index
        }) {
            return Err("duplicate reference face across shards".into());
        }
        Self::sort(&mut printings, &cards, PostingKind::Printing);
        Self::sort(&mut identities, &cards, PostingKind::Identity);
        Self::sort(&mut illustrations, &cards, PostingKind::Illustration);

        let mut total = 0_usize;
        let mut shard_ends = Vec::with_capacity(metadata.len());
        let mut ordinal_shards = Vec::with_capacity(cards.len());
        for (number, shard) in metadata.iter().enumerate() {
            total = total
                .checked_add(shard.len())
                .ok_or_else(|| "candidate metadata count overflow".to_owned())?;
            shard_ends.push(total);
            ordinal_shards.extend(std::iter::repeat_n(number, shard.len()));
        }
        Ok(Self {
            printings,
            identities,
            illustrations,
            shard_ends,
            ordinal_shards,
        })
    }

    /// Sort one flat posting by game/key while preserving enrollment order within a key.
    fn sort(postings: &mut [usize], cards: &[&CardMetadata], kind: PostingKind) {
        postings.sort_unstable_by(|&left, &right| {
            Self::compare_cards(cards[left], cards[right], kind).then(left.cmp(&right))
        });
    }

    /// Compare the game-scoped key used by one posting kind.
    fn compare_cards(left: &CardMetadata, right: &CardMetadata, kind: PostingKind) -> Ordering {
        left.game
            .cmp(&right.game)
            .then_with(|| kind.key(left).cmp(kind.key(right)))
    }

    /// Resolve a flat enrollment offset to its original shard and record offset.
    fn location(&self, ordinal: usize) -> CandidateLocation {
        let shard = self.ordinal_shards[ordinal];
        let start = shard
            .checked_sub(1)
            .map_or(0, |previous| self.shard_ends[previous]);
        CandidateLocation {
            shard,
            offset: ordinal - start,
        }
    }

    /// Resolve one exact game-scoped key through a sorted posting.
    fn get(
        &self,
        postings: &[usize],
        metadata: &[Vec<CardMetadata>],
        game: &str,
        key: &str,
        kind: PostingKind,
    ) -> Vec<CandidateLocation> {
        let compare = |ordinal: usize| {
            let location = self.location(ordinal);
            let card = &metadata[location.shard][location.offset];
            card.game
                .as_str()
                .cmp(game)
                .then_with(|| kind.key(card).cmp(key))
        };
        let start = postings.partition_point(|&ordinal| compare(ordinal).is_lt());
        let end = start + postings[start..].partition_point(|&ordinal| compare(ordinal).is_eq());
        postings[start..end]
            .iter()
            .map(|&ordinal| self.location(ordinal))
            .collect()
    }
}

/// Metadata key used by one candidate posting vector.
#[derive(Clone, Copy)]
enum PostingKind {
    /// Physical printing identifier.
    Printing,
    /// Semantic Oracle identity.
    Identity,
    /// Shared illustration identifier.
    Illustration,
}

impl PostingKind {
    /// Borrow the applicable key; absent optional keys never enter their posting.
    fn key(self, card: &CardMetadata) -> &str {
        match self {
            Self::Printing => &card.id,
            Self::Identity => card.oracle_id.as_deref().unwrap_or_default(),
            Self::Illustration => card.illustration_id.as_deref().unwrap_or_default(),
        }
    }
}

/// A native, exact catalogue backed by bounded descriptor shards on disk.
///
/// Every enrolled face participates in both printing and identity decisions.
/// Only one shard plus bounded winning candidates is held while scanning.
/// This trades repeated disk decoding for bounded descriptor memory; it is not
/// an approximate nearest-neighbor index or a low-latency whole-card database.
#[derive(Clone, Debug)]
pub struct ShardedIndex {
    /// Directory containing manifest and numbered shard files.
    directory: PathBuf,
    /// Validated catalogue publication record.
    manifest: Manifest,
    /// Binary metadata parsed once per handle and shared by inexpensive clones.
    metadata: Option<Arc<Vec<Vec<CardMetadata>>>>,
    /// Binary metadata postings for candidate-only retrieval without a gallery scan.
    candidate_lookup: Option<Arc<CandidateLookup>>,
}

impl ShardedIndex {
    /// Search all global descriptors, then verify a bounded visual/OCR candidate union.
    ///
    /// Unlike [`Matcher::recognize`], this method never runs local correspondence
    /// matching against the complete gallery unless the candidate budget covers it.
    /// Results report their evidence scope and abstain from unique geometric
    /// acceptance when omitted competitors remain possible. Hints are canonical
    /// metadata proposals, not permission to accept an OCR-only identity.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid options/input or unavailable/corrupt index files.
    pub fn recognize_shortlisted(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
        shortlist: &super::ShortlistOptions,
        hints: &[CardMetadata],
    ) -> Result<super::ShortlistedRecognition, Error> {
        super::shortlist::recognize(self, image, options, shortlist, hints)
    }

    /// Score and geometrically verify only the bounded closure of supplied candidates.
    ///
    /// Each proposal expands to its exact physical face, game-scoped semantic
    /// identity and game-scoped illustration. No complete-gallery descriptor pass
    /// occurs. The returned margins are explicitly limited to loaded candidates,
    /// and unique acceptance is suppressed whenever the selected closure does not
    /// cover the full gallery.
    ///
    /// This low-latency path requires the current split-binary index representation;
    /// legacy JSON shards cannot selectively decode individual records.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid options/input/budget, a legacy index, or
    /// unavailable/corrupt selected descriptor records.
    pub fn recognize_candidates(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
        max_candidates: usize,
        candidates: &[CardMetadata],
    ) -> Result<super::CandidateRecognition, Error> {
        super::shortlist::recognize_candidates(self, image, options, max_candidates, candidates)
    }

    /// Score a bounded candidate closure, then verify only a smaller geometric shortlist.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid settings/input, a legacy JSON index, or
    /// unavailable/corrupt selected descriptor records.
    pub fn recognize_candidates_with(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
        candidate_options: &super::CandidateOptions,
        candidates: &[CardMetadata],
    ) -> Result<super::CandidateRecognition, Error> {
        super::shortlist::recognize_candidates_with(
            self,
            image,
            options,
            candidate_options,
            candidates,
        )
    }

    /// Verify only explicitly supplied physical faces, without expanding related printings.
    ///
    /// This supports exact collector-key proposals whose ambiguity has already
    /// been retained by the text index. Returned scores and margins cover only
    /// the supplied faces; omitted gallery faces remain unsearched. Work budgets
    /// and geometry requirements are unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid options/input, legacy JSON storage, or an
    /// unavailable/corrupt selected descriptor or local-feature record.
    pub fn recognize_faces_with(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
        candidate_options: &super::CandidateOptions,
        candidates: &[CardMetadata],
    ) -> Result<super::CandidateRecognition, Error> {
        super::shortlist::recognize_faces_with(self, image, options, candidate_options, candidates)
    }

    /// Resolve one proposal through the startup-built metadata postings.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy JSON storage or a missing metadata cache.
    pub(super) fn candidate_matches(
        &self,
        candidate: &CardMetadata,
    ) -> Result<CandidateMatches, Error> {
        let lookup = self.candidate_lookup.as_ref().ok_or_else(|| {
            Error::InvalidIndex("candidate-only recognition requires a split-binary index".into())
        })?;
        let metadata = self
            .metadata
            .as_ref()
            .ok_or_else(|| Error::InvalidIndex("binary candidate metadata cache missing".into()))?;
        let printing = lookup.get(
            &lookup.printings,
            metadata,
            &candidate.game,
            &candidate.id,
            PostingKind::Printing,
        );
        let exact = printing
            .iter()
            .copied()
            .filter(|location| {
                metadata[location.shard][location.offset].face_index == candidate.face_index
            })
            .collect();
        let identity = if let Some(identity) = &candidate.oracle_id {
            lookup.get(
                &lookup.identities,
                metadata,
                &candidate.game,
                identity,
                PostingKind::Identity,
            )
        } else {
            printing
                .iter()
                .copied()
                .filter(|location| {
                    metadata[location.shard][location.offset]
                        .oracle_id
                        .is_none()
                })
                .collect()
        };
        let illustration = candidate
            .illustration_id
            .as_ref()
            .map(|illustration| {
                lookup.get(
                    &lookup.illustrations,
                    metadata,
                    &candidate.game,
                    illustration,
                    PostingKind::Illustration,
                )
            })
            .unwrap_or_default();
        Ok(CandidateMatches {
            exact,
            identity,
            illustration,
        })
    }

    /// Borrow metadata for one location produced by [`Self::candidate_matches`].
    ///
    /// # Errors
    ///
    /// Returns an error if the immutable metadata cache no longer agrees with it.
    pub(super) fn candidate_metadata(
        &self,
        location: CandidateLocation,
    ) -> Result<&CardMetadata, Error> {
        self.metadata
            .as_ref()
            .and_then(|shards| shards.get(location.shard))
            .and_then(|cards| cards.get(location.offset))
            .ok_or_else(|| Error::InvalidIndex("candidate metadata location is invalid".into()))
    }

    /// Borrow the already validated binary metadata without cloning its full catalogue.
    pub(super) fn cached_metadata(&self) -> Option<&[Vec<CardMetadata>]> {
        self.metadata.as_deref().map(Vec::as_slice)
    }

    /// Visit globally scored descriptor shards while keeping one shard resident.
    ///
    /// # Errors
    ///
    /// Returns an error for failed descriptor reads or a visitor error.
    pub(super) fn for_each_global_shard(
        &self,
        mut visit: impl FnMut(usize, CatalogIndex) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for (number, shard) in self.manifest.shards.iter().enumerate() {
            visit(number, self.read_shard(number, shard, false)?)?;
        }
        Ok(())
    }

    /// Read global and local payloads only for selected faces in a binary shard.
    ///
    /// The result keeps original shard offsets, with absent unselected faces.
    /// Legacy JSON shards must still be decoded in full before selection.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid shard selection or malformed binary evidence.
    pub(super) fn read_candidate_shard(
        &self,
        number: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Reference>>, Error> {
        let shard = self
            .manifest
            .shards
            .get(number)
            .ok_or_else(|| Error::InvalidIndex("invalid candidate shard".into()))?;
        if offsets.iter().any(|&offset| offset >= shard.references) {
            return Err(Error::InvalidIndex("candidate offset outside shard".into()));
        }
        if self.manifest.storage == Storage::SplitBinary {
            let cards = self
                .metadata
                .as_ref()
                .and_then(|shards| shards.get(number))
                .ok_or_else(|| Error::InvalidIndex("binary metadata cache missing".into()))?;
            let mut references =
                binary::globals_selected(&self.directory.join(&shard.file), cards, offsets)?;
            binary::locals_selected(
                &self.directory.join(local_filename(number)),
                &mut references,
            )?;
            Ok(references)
        } else {
            let index = self.read_shard(number, shard, true)?;
            let mut selected = vec![false; shard.references];
            for &offset in offsets {
                selected[offset] = true;
            }
            Ok(index
                .references
                .into_iter()
                .zip(selected)
                .map(|(reference, is_selected)| is_selected.then_some(reference))
                .collect())
        }
    }

    /// Read compact global payloads only for selected faces in a binary shard.
    ///
    /// The result retains original offsets and never opens the local-feature file.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, invalid offsets or malformed selected data.
    pub(super) fn read_candidate_globals(
        &self,
        number: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Reference>>, Error> {
        let shard = self
            .manifest
            .shards
            .get(number)
            .ok_or_else(|| Error::InvalidIndex("invalid candidate shard".into()))?;
        if self.manifest.storage != Storage::SplitBinary {
            return Err(Error::InvalidIndex(
                "candidate-only recognition requires a split-binary index".into(),
            ));
        }
        let cards = self
            .metadata
            .as_ref()
            .and_then(|shards| shards.get(number))
            .ok_or_else(|| Error::InvalidIndex("binary metadata cache missing".into()))?;
        binary::globals_selected(&self.directory.join(&shard.file), cards, offsets)
    }

    /// Read local feature banks only for selected faces in a binary shard.
    ///
    /// # Errors
    ///
    /// Returns an error for legacy storage, invalid offsets or malformed selected data.
    pub(super) fn read_candidate_local_features(
        &self,
        number: usize,
        offsets: &[usize],
    ) -> Result<Vec<Option<Vec<local::Feature>>>, Error> {
        let shard = self
            .manifest
            .shards
            .get(number)
            .ok_or_else(|| Error::InvalidIndex("invalid candidate shard".into()))?;
        if self.manifest.storage != Storage::SplitBinary {
            return Err(Error::InvalidIndex(
                "candidate-only recognition requires a split-binary index".into(),
            ));
        }
        binary::local_features_selected(
            &self.directory.join(local_filename(number)),
            shard.references,
            offsets,
        )
    }

    /// Open a completed sharded catalogue using structural validation only.
    ///
    /// Metadata, duplicate face keys, fixed binary headers, dimensions and file
    /// sizes are validated immediately. Descriptor vectors and variable-length
    /// local records are decoded and validated only when a search selects them.
    /// Use [`Self::load_validated`] for an offline audit of every stored record.
    ///
    /// # Errors
    ///
    /// Returns an error for missing files, unsupported or inconsistent manifests,
    /// malformed structural metadata, or duplicate physical faces.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open(directory.as_ref(), MANIFEST_FILE)
    }

    /// Open a completed catalogue and decode/validate every descriptor record.
    ///
    /// This is intended for publication checks and offline diagnostics. Normal
    /// recognition should use [`Self::load`], whose selected readers retain the
    /// same validation before any descriptor contributes evidence.
    ///
    /// # Errors
    ///
    /// Returns an error for any structural, metadata, global-vector or local-feature fault.
    pub fn load_validated(directory: impl AsRef<Path>) -> Result<Self, Error> {
        let index = Self::load(directory)?;
        index.validate_all_records()?;
        Ok(index)
    }

    /// Open either the completed publication or an explicitly resumed checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid publication records or evidence files.
    fn open(directory: &Path, record: &str) -> Result<Self, Error> {
        let directory = directory.to_path_buf();
        let manifest: Manifest =
            serde_json::from_reader(BufReader::new(File::open(directory.join(record))?)).map_err(
                |error| Error::InvalidIndex(format!("could not decode shard manifest: {error}")),
            )?;
        if !matches!(
            (manifest.format_version, manifest.storage),
            (1, Storage::Json) | (MANIFEST_VERSION, Storage::SplitBinary)
        ) || (manifest.storage == Storage::SplitBinary
            && manifest.scheme.as_deref() != Some(SCHEME))
            || manifest.backend != "portable-sharded"
            || manifest.shard_size == 0
            || manifest.references == 0
            || manifest.shards.is_empty()
        {
            return Err(Error::InvalidIndex(
                "invalid sharded catalogue manifest".into(),
            ));
        }
        let mut total = 0_usize;
        for (number, shard) in manifest.shards.iter().enumerate() {
            if shard.file != filename(number, manifest.storage)
                || shard.references == 0
                || shard.references > manifest.shard_size
            {
                return Err(Error::InvalidIndex(
                    "invalid shard filename or face count".into(),
                ));
            }
            total = total
                .checked_add(shard.references)
                .ok_or_else(|| Error::InvalidIndex("shard face count overflow".into()))?;
        }
        if total != manifest.references {
            return Err(Error::InvalidIndex(
                "shard total disagrees with manifest".into(),
            ));
        }
        let mut index = Self {
            directory,
            manifest,
            metadata: None,
            candidate_lookup: None,
        };
        let mut legacy_keys = HashSet::new();
        let mut metadata = Vec::new();
        for (number, shard) in index.manifest.shards.iter().enumerate() {
            if index.manifest.storage == Storage::SplitBinary {
                let cards: Vec<CardMetadata> = serde_json::from_reader(BufReader::new(File::open(
                    index.directory.join(metadata_filename(number)),
                )?))
                .map_err(|error| Error::InvalidIndex(format!("invalid shard metadata: {error}")))?;
                if cards.len() != shard.references {
                    return Err(Error::InvalidIndex("binary metadata count mismatch".into()));
                }
                for card in &cards {
                    validate_metadata(card).map_err(Error::InvalidIndex)?;
                }
                binary::validate_global_layout(
                    &index.directory.join(&shard.file),
                    shard.references,
                )?;
                binary::validate_local_layout(
                    &index.directory.join(local_filename(number)),
                    shard.references,
                )?;
                metadata.push(cards);
            } else {
                let loaded = index.read_shard(number, shard, true)?;
                for card in loaded.metadata() {
                    legacy_keys.insert((card.game.clone(), card.id.clone(), card.face_index));
                }
            }
        }
        if index.manifest.storage == Storage::SplitBinary {
            let lookup = CandidateLookup::new(&metadata).map_err(Error::InvalidIndex)?;
            let metadata = Arc::new(metadata);
            index.candidate_lookup = Some(Arc::new(lookup));
            index.metadata = Some(metadata);
        } else if legacy_keys.len() != index.len() {
            return Err(Error::InvalidIndex(
                "duplicate reference face across shards".into(),
            ));
        }
        Ok(index)
    }

    /// Decode every global and local descriptor bank after structural open.
    ///
    /// # Errors
    ///
    /// Returns an error for any malformed descriptor record.
    fn validate_all_records(&self) -> Result<(), Error> {
        if self.manifest.storage == Storage::Json {
            return Ok(());
        }
        for (number, shard) in self.manifest.shards.iter().enumerate() {
            self.read_shard(number, shard, true)?;
        }
        Ok(())
    }

    /// Return the total number of enrolled physical faces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.manifest.references
    }

    /// Return whether this catalogue contains no physical faces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Visit metadata in enrollment order, using the binary cache or one JSON shard.
    ///
    /// # Errors
    ///
    /// Returns an error if a shard cannot be read or fails descriptor validation.
    pub fn for_each_metadata(&self, mut visit: impl FnMut(&CardMetadata)) -> Result<(), Error> {
        if let Some(shards) = &self.metadata {
            for cards in shards.iter() {
                for card in cards {
                    visit(card);
                }
            }
        } else {
            for (number, shard) in self.manifest.shards.iter().enumerate() {
                let index = self.read_shard(number, shard, false)?;
                for card in index.metadata() {
                    visit(card);
                }
            }
        }
        Ok(())
    }

    /// Load and validate one shard against its manifest count.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable, invalid or unexpectedly sized shards.
    fn read_shard(
        &self,
        number: usize,
        shard: &Shard,
        geometry: bool,
    ) -> Result<CatalogIndex, Error> {
        let index = if self.manifest.storage == Storage::SplitBinary {
            let cards = self
                .metadata
                .as_ref()
                .and_then(|shards| shards.get(number))
                .ok_or_else(|| Error::InvalidIndex("binary metadata cache missing".into()))?;
            let mut index = binary::globals(&self.directory.join(&shard.file), cards)?;
            if geometry {
                binary::locals(&self.directory.join(local_filename(number)), &mut index)?;
            }
            index
        } else {
            CatalogIndex::load(self.directory.join(&shard.file))?
        };
        if index.len() != shard.references {
            return Err(Error::InvalidIndex(format!(
                "face count mismatch in {}",
                shard.file
            )));
        }
        Ok(index)
    }

    /// Apply same-illustration uncertainty across the whole stored catalogue.
    ///
    /// # Errors
    ///
    /// Returns an error when any shard cannot be read or validated.
    pub(super) fn illustration_ambiguity(&self, result: &mut Recognition) -> Result<(), Error> {
        let Some(best) = result.candidates.first() else {
            return Ok(());
        };
        let Some(illustration) = &best.card.illustration_id else {
            return Ok(());
        };
        let (mut printing_ambiguous, mut identity_ambiguous) = (false, false);
        self.for_each_metadata(|card| {
            if card.game == best.card.game
                && !same_face(card, &best.card)
                && card.illustration_id.as_ref() == Some(illustration)
            {
                printing_ambiguous = true;
                identity_ambiguous |= !same_identity(card, &best.card);
            }
        })?;
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

impl Matcher for ShardedIndex {
    fn recognize(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
    ) -> Result<Recognition, Error> {
        options.validate()?;
        validate_image(image)?;
        if !has_texture(image) {
            return Ok(Recognition {
                status: MatchStatus::Unknown,
                identity_status: MatchStatus::Unknown,
                confidence: 0.0,
                identity_confidence: 0.0,
                candidates: Vec::new(),
            });
        }
        let rgb = image.to_rgb8();
        let queries = query_descriptors(&rgb);
        let mut ranking = Ranking::new(options.top_k);
        for (number, shard) in self.manifest.shards.iter().enumerate() {
            let index = self.read_shard(number, shard, false)?;
            for reference in &index.references {
                ranking.push(score_reference(reference, &queries).0);
            }
        }
        // This gate is evaluated only after every shard has supplied global evidence.
        if ranking
            .top
            .first()
            .is_some_and(|best| best.similarity >= options.min_similarity)
        {
            return ranking.finish(options);
        }
        let query_features = local::extract(image);
        let normalized_query =
            image::imageops::resize(&rgb, local::WIDTH, local::HEIGHT, FilterType::Triangle);
        let mut ranking = Ranking::new(options.top_k);
        let mut used_geometry = false;
        for (number, shard) in self.manifest.shards.iter().enumerate() {
            let index = self.read_shard(number, shard, true)?;
            for reference in &index.references {
                let (mut candidate, mut explanation) = score_reference(reference, &queries);
                used_geometry |= verify_geometry(
                    reference,
                    &query_features,
                    &normalized_query,
                    &mut candidate,
                    &mut explanation,
                );
                ranking.push(candidate);
            }
        }
        let mut result = ranking.finish(options)?;
        if used_geometry && result.status == MatchStatus::Matched {
            self.illustration_ambiguity(&mut result)?;
        }
        Ok(result)
    }
}

/// Incremental builder retaining at most `shard_size` faces' visual descriptors.
///
/// A compact set of enrolled face keys prevents duplicates across shards; that
/// registry grows with the number of faces, independently of descriptor memory.
/// Failed or interrupted builds have no manifest and cannot be opened as complete.
pub struct ShardedIndexBuilder {
    /// Directory reserved for this build.
    directory: PathBuf,
    /// At most one shard of pending reference descriptors.
    pending: CatalogIndex,
    /// Publication record accumulated while writing shards.
    manifest: Manifest,
    /// Global face registry preventing duplicates across already-flushed shards.
    keys: HashSet<(String, String, u32)>,
    /// Completed binary shard metadata retained for the returned matcher.
    metadata: Vec<Vec<CardMetadata>>,
}

impl ShardedIndexBuilder {
    /// Start a new build in an absent or empty output directory.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero shard size, a nonempty output directory, or
    /// filesystem errors. Existing catalogue files are never overwritten.
    pub fn new(directory: impl AsRef<Path>, shard_size: usize) -> Result<Self, Error> {
        Self::with_storage(directory, shard_size, Storage::SplitBinary)
    }

    /// Reserve a fresh build using a selected representation.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid settings or an occupied output directory.
    fn with_storage(
        directory: impl AsRef<Path>,
        shard_size: usize,
        storage: Storage,
    ) -> Result<Self, Error> {
        if shard_size == 0 {
            return Err(Error::InvalidOptions("shard size must be positive".into()));
        }
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;
        if fs::read_dir(&directory)?.next().is_some() {
            return Err(Error::InvalidCatalog(
                "sharded output directory must be empty".into(),
            ));
        }
        Ok(Self {
            directory,
            pending: CatalogIndex::new(),
            manifest: Manifest {
                format_version: if storage == Storage::Json {
                    1
                } else {
                    MANIFEST_VERSION
                },
                backend: "portable-sharded".into(),
                storage,
                scheme: if storage == Storage::SplitBinary {
                    Some(SCHEME.into())
                } else {
                    None
                },
                references: 0,
                shard_size,
                shards: Vec::new(),
            },
            keys: HashSet::new(),
            metadata: Vec::new(),
        })
    }

    /// Resume an interrupted build from its last fully flushed descriptor shard.
    ///
    /// Unflushed faces must be supplied again. Use [`Self::contains`] to skip
    /// completed face keys before decoding their source images.
    ///
    /// # Errors
    ///
    /// Returns an error for a completed build, missing checkpoint or corrupt shards.
    pub fn resume(directory: impl AsRef<Path>) -> Result<Self, Error> {
        let directory = directory.as_ref();
        if directory.join(MANIFEST_FILE).exists() {
            return Err(Error::InvalidCatalog(
                "catalogue is already complete".into(),
            ));
        }
        let index = ShardedIndex::open(directory, CHECKPOINT_FILE)?;
        index.validate_all_records()?;
        let mut keys = HashSet::new();
        index.for_each_metadata(|card| {
            keys.insert((card.game.clone(), card.id.clone(), card.face_index));
        })?;
        Ok(Self {
            directory: index.directory,
            pending: CatalogIndex::new(),
            manifest: index.manifest,
            keys,
            metadata: index.metadata.map(Arc::unwrap_or_clone).unwrap_or_default(),
        })
    }

    /// Return the number of faces enrolled, including the pending incomplete shard.
    #[must_use]
    pub fn len(&self) -> usize {
        self.manifest.references
    }

    /// Return whether no physical faces have been enrolled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Check whether a physical face is already enrolled, before reading its image.
    #[must_use]
    pub fn contains(&self, card: &CardMetadata) -> bool {
        self.keys
            .contains(&(card.game.clone(), card.id.clone(), card.face_index))
    }

    /// Enroll one upright physical face, flushing a full descriptor shard to disk.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate or invalid metadata, invalid image evidence,
    /// descriptor extraction failure, or shard-file write errors.
    pub fn add(&mut self, card: CardMetadata, image: &DynamicImage) -> Result<(), Error> {
        let key = (card.game.clone(), card.id.clone(), card.face_index);
        if self.keys.contains(&key) {
            return Err(Error::InvalidCatalog(
                "duplicate reference face across shards".into(),
            ));
        }
        self.commit(prepare(card, image)?)
    }

    /// Extract a bounded batch concurrently, then enroll it in the supplied order.
    ///
    /// Worker count affects throughput, not descriptor values or tie ordering.
    /// The caller controls image memory through the size of `faces`. A batch is
    /// fully prepared before enrollment, so invalid images do not partially add it.
    ///
    /// # Errors
    ///
    /// Returns an error for zero workers, duplicate faces, invalid image evidence,
    /// worker failure or shard writes. A write failure may leave completed shards
    /// recoverable through [`Self::resume`].
    pub fn add_batch(
        &mut self,
        faces: Vec<(CardMetadata, DynamicImage)>,
        workers: usize,
    ) -> Result<(), Error> {
        if workers == 0 {
            return Err(Error::InvalidOptions(
                "index workers must be positive".into(),
            ));
        }
        let mut batch_keys = HashSet::new();
        for (card, _) in &faces {
            let key = (card.game.clone(), card.id.clone(), card.face_index);
            if self.keys.contains(&key) || !batch_keys.insert(key) {
                return Err(Error::InvalidCatalog(
                    "duplicate reference face in enrollment batch".into(),
                ));
            }
        }
        if faces.is_empty() {
            return Ok(());
        }
        let size = faces.len().div_ceil(workers);
        let prepared = std::thread::scope(|scope| {
            let mut jobs = faces.into_iter();
            let mut handles = Vec::new();
            loop {
                let batch: Vec<_> = jobs.by_ref().take(size).collect();
                if batch.is_empty() {
                    break;
                }
                handles.push(scope.spawn(move || {
                    batch
                        .into_iter()
                        .map(|(card, image)| prepare(card, &image))
                        .collect::<Result<Vec<_>, Error>>()
                }));
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        Error::Backend("parallel reference extraction worker failed".into())
                    })?
                })
                .collect::<Result<Vec<_>, Error>>()
        })?;
        for batch in prepared {
            for reference in batch {
                self.commit(reference)?;
            }
        }
        Ok(())
    }

    /// Commit already validated evidence without repeating image feature extraction.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate metadata or failed shard publication.
    fn commit(&mut self, reference: Reference) -> Result<(), Error> {
        let key = (
            reference.card.game.clone(),
            reference.card.id.clone(),
            reference.card.face_index,
        );
        if !self.keys.insert(key) {
            return Err(Error::InvalidCatalog(
                "duplicate reference face across shards".into(),
            ));
        }
        self.pending.references.push(reference);
        self.manifest.references += 1;
        if self.pending.len() == self.manifest.shard_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush remaining faces and publish the completed manifest atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty catalogue or unwritable shard/manifest files.
    pub fn finish(mut self) -> Result<ShardedIndex, Error> {
        if self.manifest.references == 0 {
            return Err(Error::InvalidCatalog("reference index is empty".into()));
        }
        self.flush()?;
        let candidate_lookup = if self.manifest.storage == Storage::SplitBinary {
            Some(Arc::new(
                CandidateLookup::new(&self.metadata).map_err(Error::InvalidCatalog)?,
            ))
        } else {
            None
        };
        publish(&self.directory, MANIFEST_FILE, &self.manifest)?;
        fs::remove_file(self.directory.join(CHECKPOINT_FILE))?;
        Ok(ShardedIndex {
            directory: self.directory,
            candidate_lookup,
            metadata: if self.manifest.storage == Storage::SplitBinary {
                Some(Arc::new(self.metadata))
            } else {
                None
            },
            manifest: self.manifest,
        })
    }

    /// Save one full or final partial shard without duplicating its descriptors.
    ///
    /// # Errors
    ///
    /// Returns an error when the shard cannot be written or renamed.
    fn flush(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let number = self.manifest.shards.len();
        let file = filename(number, self.manifest.storage);
        let temporary = self.directory.join(format!("{file}.tmp"));
        if self.manifest.storage == Storage::SplitBinary {
            let local_file = local_filename(number);
            let local_temporary = self.directory.join(format!("{local_file}.tmp"));
            binary::save(&self.pending, &temporary, &local_temporary)?;
            fs::rename(local_temporary, self.directory.join(local_file))?;
            let cards: Vec<_> = self.pending.metadata().cloned().collect();
            let metadata_file = metadata_filename(number);
            let metadata_temporary = self.directory.join(format!("{metadata_file}.tmp"));
            let mut writer = BufWriter::new(File::create(&metadata_temporary)?);
            serde_json::to_writer(&mut writer, &cards)?;
            writer.flush()?;
            drop(writer);
            fs::rename(metadata_temporary, self.directory.join(metadata_file))?;
            self.metadata.push(cards);
        } else {
            self.pending.save(&temporary)?;
        }
        fs::rename(temporary, self.directory.join(&file))?;
        self.manifest.shards.push(Shard {
            file,
            references: self.pending.len(),
        });
        self.pending = CatalogIndex::new();
        publish(&self.directory, CHECKPOINT_FILE, &self.manifest)?;
        Ok(())
    }
}

/// Extract one validated reference independently from mutable builder state.
///
/// # Errors
///
/// Returns an error for invalid metadata or insufficient image evidence.
fn prepare(card: CardMetadata, image: &DynamicImage) -> Result<Reference, Error> {
    let mut index = CatalogIndex::new();
    index.add(card, image)?;
    index
        .references
        .pop()
        .ok_or_else(|| Error::InvalidCatalog("reference extraction produced no evidence".into()))
}

/// Atomically publish a manifest or recoverable completed-shard checkpoint.
///
/// # Errors
///
/// Returns an error for unwritable publication files.
fn publish(directory: &Path, name: &str, manifest: &Manifest) -> Result<(), Error> {
    let temporary = directory.join(format!("{name}.tmp"));
    let mut writer = BufWriter::new(File::create(&temporary)?);
    serde_json::to_writer(&mut writer, manifest)?;
    writer.flush()?;
    drop(writer);
    fs::rename(temporary, directory.join(name))?;
    Ok(())
}

/// Resolve the generated descriptor filename for the selected representation.
fn filename(number: usize, storage: Storage) -> String {
    match storage {
        Storage::Json => shard_filename(number),
        Storage::SplitBinary => format!("shard-{number:06}.global.bin"),
    }
}

/// Generate the local-feature path independently from global evidence.
fn local_filename(number: usize) -> String {
    format!("shard-{number:06}.local.bin")
}

/// Generate the small metadata path, which is decoded once when opening a handle.
fn metadata_filename(number: usize) -> String {
    format!("shard-{number:06}.metadata.json")
}

/// Generate filenames without accepting manifest-supplied path traversal.
fn shard_filename(number: usize) -> String {
    format!("shard-{number:06}.json")
}

/// Regression coverage for complete-gallery decisions and bounded descriptor storage.
#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Parallel tests reserve distinct output directories.
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    /// A temporary catalogue removed after each test, including failed builds.
    struct Directory(PathBuf);

    impl Directory {
        /// Choose a unique absent directory under the system temporary root.
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "whosthatpokemon-sharded-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Deterministic card-like images with shared frames and distinctive artwork.
    fn image(seed: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(168, 234, |x, y| {
            if !(6..162).contains(&x) || !(6..228).contains(&y) {
                Rgb([12, 12, 12])
            } else if (38..134).contains(&y) {
                let value = x.wrapping_mul(41 + seed * 19)
                    ^ y.wrapping_mul(73 + seed * 31)
                    ^ (x / (3 + seed % 4) * y / 7).wrapping_mul(53);
                Rgb([
                    (value & 255) as u8,
                    ((value >> 3) & 255) as u8,
                    ((value >> 6) & 255) as u8,
                ])
            } else if (145..215).contains(&y) && y % 11 < 2 {
                Rgb([35, 35, 35])
            } else {
                Rgb([190, 182, 155])
            }
        }))
    }

    /// Simple card metadata with an explicit semantic identity and shared art ID.
    fn card(id: &str, oracle: &str, illustration: &str) -> CardMetadata {
        CardMetadata {
            game: "mtg".into(),
            id: id.into(),
            name: format!("Card {oracle}"),
            oracle_id: Some(oracle.into()),
            illustration_id: Some(illustration.into()),
            ..CardMetadata::default()
        }
    }

    /// Flat postings retain enrollment order, shard locations and game scoping.
    #[test]
    fn candidate_lookup_preserves_exact_posting_semantics() {
        let mut front = card("two-face", "shared", "same-art");
        let mut back = front.clone();
        back.face_index = 1;
        let reprint = card("earlier-name", "shared", "other-art");
        let mut other_game = card("two-face", "shared", "same-art");
        other_game.game = "other".into();
        let metadata = vec![vec![front.clone(), back], vec![reprint], vec![other_game]];
        let lookup = CandidateLookup::new(&metadata).unwrap();

        assert_eq!(
            lookup.get(
                &lookup.printings,
                &metadata,
                "mtg",
                "two-face",
                PostingKind::Printing,
            ),
            [
                CandidateLocation {
                    shard: 0,
                    offset: 0,
                },
                CandidateLocation {
                    shard: 0,
                    offset: 1,
                },
            ]
        );
        assert_eq!(
            lookup.get(
                &lookup.identities,
                &metadata,
                "mtg",
                "shared",
                PostingKind::Identity,
            ),
            [
                CandidateLocation {
                    shard: 0,
                    offset: 0,
                },
                CandidateLocation {
                    shard: 0,
                    offset: 1,
                },
                CandidateLocation {
                    shard: 1,
                    offset: 0,
                },
            ]
        );
        assert_eq!(
            lookup.get(
                &lookup.illustrations,
                &metadata,
                "mtg",
                "same-art",
                PostingKind::Illustration,
            ),
            [
                CandidateLocation {
                    shard: 0,
                    offset: 0,
                },
                CandidateLocation {
                    shard: 0,
                    offset: 1,
                },
            ]
        );

        front.face_index = 1;
        let duplicate = vec![metadata[0].clone(), vec![front]];
        assert!(CandidateLookup::new(&duplicate).is_err());
    }

    /// Build both backends from identical enrollment order with one face per shard.
    fn catalogues(directory: &Directory) -> (CatalogIndex, ShardedIndex) {
        catalogues_with_storage(directory, Storage::Json)
    }

    /// Enroll the same fixture using either backward-compatible JSON or split binary.
    fn catalogues_with_storage(
        directory: &Directory,
        storage: Storage,
    ) -> (CatalogIndex, ShardedIndex) {
        let mut memory = CatalogIndex::new();
        let mut builder = ShardedIndexBuilder::with_storage(&directory.0, 1, storage).unwrap();
        for (id, oracle, illustration, seed) in [
            ("first", "alpha", "art-a", 1),
            ("reprint", "alpha", "art-a", 1),
            ("competitor", "beta", "art-b", 7),
        ] {
            memory
                .add(card(id, oracle, illustration), &image(seed))
                .unwrap();
            builder
                .add(card(id, oracle, illustration), &image(seed))
                .unwrap();
            assert!(
                builder.pending.is_empty(),
                "full descriptor shard must be released"
            );
        }
        (memory, builder.finish().unwrap())
    }

    /// Native binary values preserve scores exactly and omit local I/O on global matches.
    #[test]
    fn split_binary_round_trips_exactly_and_caches_metadata() {
        let directory = Directory::new();
        let (memory, disk) = catalogues_with_storage(&directory, Storage::SplitBinary);
        let loaded = ShardedIndex::load(&directory.0).unwrap();
        for query in [image(1), image(7).rotate90(), image(19)] {
            let options = MatchOptions {
                top_k: 1,
                ..MatchOptions::default()
            };
            assert_eq!(
                disk.recognize(&query, &options).unwrap(),
                memory.recognize(&query, &options).unwrap()
            );
            assert_eq!(
                loaded.recognize(&query, &options).unwrap(),
                memory.recognize(&query, &options).unwrap()
            );
        }
        // Open structurally checked all files. Subsequent globally accepted queries
        // use only globals; metadata callbacks use the cache. Missing locals still
        // fail a geometry fallback when it is actually requested.
        for number in 0..disk.manifest.shards.len() {
            fs::remove_file(directory.0.join(local_filename(number))).unwrap();
            fs::remove_file(directory.0.join(metadata_filename(number))).unwrap();
        }
        let mut count = 0;
        loaded.for_each_metadata(|_| count += 1).unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            loaded
                .recognize(&image(7), &MatchOptions::default())
                .unwrap(),
            memory
                .recognize(&image(7), &MatchOptions::default())
                .unwrap()
        );
        assert!(
            loaded
                .recognize(&image(19), &MatchOptions::default())
                .is_err()
        );
    }

    /// Bounded geometry sees the same selected evidence at unchanged shard offsets.
    #[test]
    fn selected_candidate_banks_match_full_shard_for_binary_and_json() {
        for storage in [Storage::SplitBinary, Storage::Json] {
            let directory = Directory::new();
            let mut builder = ShardedIndexBuilder::with_storage(&directory.0, 4, storage).unwrap();
            for (id, seed) in [("first", 1), ("second", 7), ("third", 11), ("fourth", 19)] {
                builder.add(card(id, id, id), &image(seed)).unwrap();
            }
            let disk = builder.finish().unwrap();
            let full = disk.read_shard(0, &disk.manifest.shards[0], true).unwrap();
            let selected = disk.read_candidate_shard(0, &[3, 1, 1]).unwrap();
            assert_eq!(selected.len(), 4);
            assert!(selected[0].is_none());
            assert!(selected[2].is_none());
            for offset in [1, 3] {
                let candidate = selected[offset].as_ref().unwrap();
                let original = &full.references[offset];
                assert_eq!(
                    serde_json::to_value(candidate).unwrap(),
                    serde_json::to_value(original).unwrap()
                );
                for (actual, expected) in candidate.views.iter().zip(&original.views) {
                    assert_eq!(actual.full_norm.to_bits(), expected.full_norm.to_bits());
                    assert_eq!(actual.art_norm.to_bits(), expected.art_norm.to_bits());
                    assert_eq!(
                        actual.histogram_norm.to_bits(),
                        expected.histogram_norm.to_bits()
                    );
                }
            }
            assert!(disk.read_candidate_shard(0, &[4]).is_err());
            if storage == Storage::SplitBinary {
                let path = directory.0.join(&disk.manifest.shards[0].file);
                let original = fs::read(&path).unwrap();
                let mut bad = original.clone();
                bad[0] ^= 1;
                fs::write(&path, bad).unwrap();
                assert!(disk.read_candidate_shard(0, &[1]).is_err());
                let mut bad = original.clone();
                let selected_record = binary::GLOBAL_HEADER as usize + binary::GLOBAL_BYTES;
                bad[selected_record..selected_record + 2].copy_from_slice(&i16::MIN.to_le_bytes());
                fs::write(&path, bad).unwrap();
                assert!(disk.read_candidate_shard(0, &[1]).is_err());
                fs::write(&path, &original[..original.len() - 1]).unwrap();
                assert!(disk.read_candidate_shard(0, &[1]).is_err());
            }
        }
    }

    /// Fast open checks structure while selected/deep reads validate record contents.
    #[test]
    fn lazy_and_deep_load_validate_binary_evidence_at_their_promised_scope() {
        let directory = Directory::new();
        let (_, disk) = catalogues_with_storage(&directory, Storage::SplitBinary);
        let global_path = directory.0.join(&disk.manifest.shards[2].file);
        let local_path = directory.0.join(local_filename(2));
        let global = fs::read(&global_path).unwrap();
        let local = fs::read(&local_path).unwrap();
        let mut corruptions = Vec::new();
        let mut bad = global.clone();
        bad[0] ^= 1;
        corruptions.push(bad);
        let mut bad = global.clone();
        bad[8..12].copy_from_slice(&999_u32.to_le_bytes());
        corruptions.push(bad);
        let mut bad = global.clone();
        bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        corruptions.push(bad);
        let mut bad = global.clone();
        bad.truncate(bad.len() - 1);
        corruptions.push(bad);
        let mut bad = global.clone();
        bad.push(0);
        corruptions.push(bad);
        for bad in corruptions {
            fs::write(&global_path, bad).unwrap();
            assert!(ShardedIndex::load(&directory.0).is_err());
        }
        fs::write(&global_path, &global).unwrap();

        let mut bad = fs::read(&global_path).unwrap();
        bad[36..38].copy_from_slice(&i16::MIN.to_le_bytes());
        fs::write(&global_path, bad).unwrap();
        let lazy = ShardedIndex::load(&directory.0).unwrap();
        assert!(ShardedIndex::load_validated(&directory.0).is_err());
        assert!(
            lazy.recognize_candidates(
                &image(7),
                &MatchOptions::default(),
                1,
                &[card("competitor", "beta", "art-b")],
            )
            .is_err()
        );
        fs::write(&global_path, &global).unwrap();

        let mut corruptions = Vec::new();
        let mut bad = local.clone();
        bad[0] ^= 1;
        corruptions.push(bad);
        let mut bad = local.clone();
        bad[8..12].copy_from_slice(&999_u32.to_le_bytes());
        corruptions.push(bad);
        let mut bad = local.clone();
        bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        corruptions.push(bad);
        let mut bad = local.clone();
        bad.truncate(15);
        corruptions.push(bad);
        for bad in corruptions {
            fs::write(&local_path, bad).unwrap();
            assert!(ShardedIndex::load(&directory.0).is_err());
        }
        fs::write(&local_path, &local).unwrap();

        let mut record_corruptions = Vec::new();
        let mut bad = local.clone();
        bad[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        record_corruptions.push(bad);
        let mut bad = local.clone();
        bad[20..22].copy_from_slice(&u16::MAX.to_le_bytes());
        record_corruptions.push(bad);
        for bad in record_corruptions {
            fs::write(&local_path, bad).unwrap();
            let lazy = ShardedIndex::load(&directory.0).unwrap();
            assert!(ShardedIndex::load_validated(&directory.0).is_err());
            assert!(
                lazy.recognize_candidates(
                    &image(7),
                    &MatchOptions::default(),
                    1,
                    &[card("competitor", "beta", "art-b")],
                )
                .is_err()
            );
        }
    }

    /// Parallel extraction writes byte-identical evidence in original enrollment order.
    #[test]
    fn parallel_batches_preserve_serial_descriptors_and_ties() {
        let serial_directory = Directory::new();
        let parallel_directory = Directory::new();
        let mut serial = ShardedIndexBuilder::new(&serial_directory.0, 2).unwrap();
        let mut parallel = ShardedIndexBuilder::new(&parallel_directory.0, 2).unwrap();
        let faces = vec![
            (card("first", "alpha", "a"), image(1)),
            (card("second", "alpha", "a"), image(1)),
            (card("third", "beta", "b"), image(7)),
        ];
        for (card, image) in &faces {
            serial.add(card.clone(), image).unwrap();
        }
        parallel.add_batch(faces, 2).unwrap();
        let serial = serial.finish().unwrap();
        let parallel = parallel.finish().unwrap();
        for number in 0..serial.manifest.shards.len() {
            for name in [
                filename(number, Storage::SplitBinary),
                local_filename(number),
                metadata_filename(number),
            ] {
                assert_eq!(
                    fs::read(serial_directory.0.join(&name)).unwrap(),
                    fs::read(parallel_directory.0.join(&name)).unwrap()
                );
            }
        }
        let options = MatchOptions {
            top_k: 1,
            ..MatchOptions::default()
        };
        assert_eq!(
            serial.recognize(&image(1), &options).unwrap(),
            parallel.recognize(&image(1), &options).unwrap()
        );
    }

    /// Interrupted enrollment resumes completed shards and requires pending faces again.
    #[test]
    fn resumes_checkpoint_without_reenrolling_completed_faces() {
        let directory = Directory::new();
        let mut builder = ShardedIndexBuilder::new(&directory.0, 2).unwrap();
        builder
            .add(card("first", "alpha", "art-a"), &image(1))
            .unwrap();
        builder
            .add(card("second", "beta", "art-b"), &image(7))
            .unwrap();
        builder
            .add(card("pending", "gamma", "art-c"), &image(11))
            .unwrap();
        assert_eq!(builder.len(), 3);
        drop(builder);
        assert!(ShardedIndex::load(&directory.0).is_err());
        let mut builder = ShardedIndexBuilder::resume(&directory.0).unwrap();
        assert_eq!(builder.len(), 2);
        assert!(builder.contains(&card("first", "alpha", "art-a")));
        assert!(!builder.contains(&card("pending", "gamma", "art-c")));
        builder
            .add(card("pending", "gamma", "art-c"), &image(11))
            .unwrap();
        let disk = builder.finish().unwrap();
        assert_eq!(disk.len(), 3);
        assert!(!directory.0.join(CHECKPOINT_FILE).exists());
        assert!(ShardedIndexBuilder::resume(&directory.0).is_err());
        assert_eq!(
            disk.recognize(&image(11), &MatchOptions::default())
                .unwrap()
                .candidates[0]
                .card
                .id,
            "pending"
        );
    }

    /// Stable ties, rotations and identity margins survive shard boundaries/top-k truncation.
    #[test]
    fn matches_full_gallery_printing_and_identity_decisions() {
        let directory = Directory::new();
        let (memory, disk) = catalogues(&directory);
        let loaded = ShardedIndex::load(&directory.0).unwrap();
        for top_k in [1, 2, 5] {
            for query in [
                image(1),
                image(7).brighten(12).rotate90(),
                DynamicImage::new_rgb8(5, 8),
            ] {
                let options = MatchOptions {
                    top_k,
                    ..MatchOptions::default()
                };
                let expected = memory.recognize(&query, &options).unwrap();
                assert_eq!(disk.recognize(&query, &options).unwrap(), expected);
                assert_eq!(loaded.recognize(&query, &options).unwrap(), expected);
            }
        }
        let result = disk
            .recognize(
                &image(1),
                &MatchOptions {
                    top_k: 1,
                    ..MatchOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.status, MatchStatus::Ambiguous);
        assert_eq!(result.identity_status, MatchStatus::Matched);
        assert_eq!(result.candidates[0].card.id, "first");
        assert_eq!(disk.len(), 3);
        let mut ids = Vec::new();
        disk.for_each_metadata(|card| ids.push(card.id.clone()))
            .unwrap();
        assert_eq!(ids, ["first", "reprint", "competitor"]);
    }

    /// Global evidence from later shards prevents premature per-shard geometry fallback.
    #[test]
    fn global_gate_and_unknown_fallback_match_in_memory_catalogue() {
        let directory = Directory::new();
        let (memory, disk) = catalogues(&directory);
        for query in [image(7), image(19)] {
            let options = MatchOptions::default();
            assert_eq!(
                disk.recognize(&query, &options).unwrap(),
                memory.recognize(&query, &options).unwrap()
            );
        }
        // A near-unity gate forces both complete-gallery scans through geometry.
        let query = image(7).blur(0.6).brighten(-18);
        let options = MatchOptions {
            min_similarity: 0.99999,
            ..MatchOptions::default()
        };
        assert_eq!(
            disk.recognize(&query, &options).unwrap(),
            memory.recognize(&query, &options).unwrap()
        );
    }

    /// An early shard's possible geometry uplift cannot override a later global match.
    #[test]
    fn later_global_match_suppresses_earlier_shards_geometric_uplift() {
        let directory = Directory::new();
        let (mut memory, disk) = catalogues(&directory);
        for view in &mut memory.references[0].views {
            for value in &mut view.full {
                *value = -*value;
            }
        }
        CatalogIndex {
            references: vec![memory.references[0].clone()],
        }
        .save(directory.0.join(&disk.manifest.shards[0].file))
        .unwrap();
        let options = MatchOptions {
            min_similarity: 0.9,
            top_k: 1,
            ..MatchOptions::default()
        };
        // In isolation the first shard would run geometry and rescue the first printing.
        let isolated = CatalogIndex {
            references: vec![memory.references[0].clone()],
        };
        let (_, evidence) = isolated.recognize_explained(&image(1), &options).unwrap();
        assert_eq!(evidence[0].method, "verified_geometry");
        // The later unmodified reprint passes the global gate, so the complete
        // catalogue must skip that rescue and preserve the globally winning printing.
        let expected = memory.recognize(&image(1), &options).unwrap();
        assert_eq!(expected.status, MatchStatus::Matched);
        assert_eq!(expected.candidates[0].card.id, "reprint");
        assert_eq!(disk.recognize(&image(1), &options).unwrap(), expected);
    }

    /// Verified geometry still checks shared illustrations in distant catalogue shards.
    #[test]
    fn geometric_winner_retains_complete_gallery_illustration_ambiguity() {
        let directory = Directory::new();
        let (mut memory, _) = catalogues(&directory);
        memory.references.remove(1);
        memory.references[1].card.illustration_id = Some("art-a".into());
        // Remove the global full-face correlation while preserving the artwork
        // evidence and exact local correspondences used by the geometry stage.
        for reference in &mut memory.references {
            for view in &mut reference.views {
                for value in &mut view.full {
                    *value = -*value;
                }
            }
        }
        let mut manifest = Manifest {
            format_version: 1,
            backend: "portable-sharded".into(),
            storage: Storage::Json,
            scheme: None,
            references: memory.len(),
            shard_size: 1,
            shards: Vec::new(),
        };
        for (number, reference) in memory.references.iter().enumerate() {
            let file = shard_filename(number);
            CatalogIndex {
                references: vec![reference.clone()],
            }
            .save(directory.0.join(&file))
            .unwrap();
            manifest.shards.push(Shard {
                file,
                references: 1,
            });
        }
        fs::write(
            directory.0.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let disk = ShardedIndex::load(&directory.0).unwrap();
        let options = MatchOptions {
            min_similarity: 0.9,
            top_k: 1,
            ..MatchOptions::default()
        };
        let (expected, evidence) = memory.recognize_explained(&image(1), &options).unwrap();
        assert_eq!(evidence[0].method, "verified_geometry");
        assert_eq!(expected.status, MatchStatus::Ambiguous);
        assert_eq!(expected.identity_status, MatchStatus::Ambiguous);
        assert_eq!(disk.recognize(&image(1), &options).unwrap(), expected);
    }

    /// Runner-up sufficient statistics remain exact for arbitrary ties and identity order.
    #[test]
    fn bounded_ranking_preserves_complete_gallery_margins() {
        for top_k in [1, 2, 5, 100] {
            let options = MatchOptions {
                top_k,
                ..MatchOptions::default()
            };
            for order in 0..7 {
                let mut all = Vec::new();
                let mut ranking = Ranking::new(top_k);
                for index in 0..127 {
                    let candidate = MatchCandidate {
                        card: card(
                            &format!("face-{index}"),
                            &format!("identity-{}", (index * 13 + order) % 7),
                            "art",
                        ),
                        similarity: ((index * 31 + order * 7) % 17) as f32 / 17.0,
                        confidence: 0.0,
                        rotation_degrees: 0,
                    };
                    all.push(candidate.clone());
                    ranking.push(candidate);
                    assert!(ranking.top.len() <= top_k.max(2));
                    assert!(ranking.identities.len() <= 2);
                }
                assert_eq!(
                    ranking.finish(&options).unwrap(),
                    finish_recognition(all, &options).unwrap()
                );
            }
        }
    }

    /// Publication and global uniqueness prevent partially built/duplicate catalogues.
    #[test]
    fn rejects_duplicate_faces_and_unfinished_or_existing_builds() {
        let directory = Directory::new();
        assert!(ShardedIndexBuilder::new(&directory.0, 0).is_err());
        let mut builder =
            ShardedIndexBuilder::with_storage(&directory.0, 1, Storage::Json).unwrap();
        builder
            .add(card("first", "alpha", "art"), &image(1))
            .unwrap();
        assert!(ShardedIndex::load(&directory.0).is_err());
        assert!(
            builder
                .add(card("first", "alpha", "art"), &image(7))
                .is_err()
        );
        assert!(ShardedIndexBuilder::new(&directory.0, 1).is_err());
        let disk = builder.finish().unwrap();
        assert_eq!(disk.len(), 1);
        let empty = Directory::new();
        assert!(
            ShardedIndexBuilder::new(&empty.0, 1)
                .unwrap()
                .finish()
                .is_err()
        );
    }

    /// Late-shard corruption and cross-shard duplicates cannot silently narrow the gallery.
    #[test]
    fn rejects_corrupt_or_duplicate_late_shards_and_manifest_paths() {
        let directory = Directory::new();
        let (_, disk) = catalogues(&directory);
        let late = directory.0.join(&disk.manifest.shards[2].file);
        let original = fs::read(&late).unwrap();
        fs::write(&late, b"{}").unwrap();
        assert!(ShardedIndex::load(&directory.0).is_err());
        assert!(disk.recognize(&image(1), &MatchOptions::default()).is_err());
        fs::copy(directory.0.join(&disk.manifest.shards[0].file), &late).unwrap();
        assert!(ShardedIndex::load(&directory.0).is_err());
        fs::write(&late, original).unwrap();
        let mut manifest = disk.manifest.clone();
        manifest.shards[2].file = "../outside.json".into();
        fs::write(
            directory.0.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(ShardedIndex::load(&directory.0).is_err());
    }
}
