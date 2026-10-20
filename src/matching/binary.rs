//! Bounded, lossless native files for exact global retrieval and optional geometry.

use super::{
    ART_HEIGHT, ART_WIDTH, CatalogIndex, Descriptor, FULL_HEIGHT, FULL_WIDTH, HASH_BITS, HUE_BINS,
    INSETS, Reference, SATURATION_BINS, local,
};
use crate::{CardMetadata, Error};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, Write},
    path::Path,
};

/// Global file identifier; includes a representation revision independent of JSON.
const GLOBAL_MAGIC: &[u8; 8] = b"WTPGLB01";
/// Local-feature file identifier.
const LOCAL_MAGIC: &[u8; 8] = b"WTPLOC01";
/// Binary representation revision.
const VERSION: u32 = 1;
/// Number of bytes in one unchanged five-view global descriptor bank.
pub(super) const GLOBAL_BYTES: usize = INSETS.len()
    * (((FULL_WIDTH * FULL_HEIGHT + ART_WIDTH * ART_HEIGHT) as usize + HUE_BINS * SATURATION_BINS)
        * 2
        + 8);
/// Global header bytes: magic plus version/count/view count/vector dimensions/hash bits.
pub(super) const GLOBAL_HEADER: u64 = 36;
/// Local header bytes: magic plus version and record count.
const LOCAL_HEADER: u64 = 16;
/// Maximum local feature count, matching the existing extractor's validation bound.
const MAX_FEATURES: usize = 1020;
/// Encoded bytes per local keypoint and binary descriptor.
const FEATURE_BYTES: usize = 36;

/// Write separate fixed-width global evidence and variable-length local evidence.
///
/// # Errors
///
/// Returns an error for unwritable files or a face count exceeding the format limit.
pub(super) fn save(index: &CatalogIndex, global: &Path, local_path: &Path) -> Result<(), Error> {
    let count = u32::try_from(index.len())
        .map_err(|_| Error::InvalidCatalog("too many faces in binary shard".into()))?;
    let mut writer = BufWriter::new(File::create(global)?);
    writer.write_all(GLOBAL_MAGIC)?;
    for value in [
        VERSION,
        count,
        INSETS.len() as u32,
        FULL_WIDTH * FULL_HEIGHT,
        ART_WIDTH * ART_HEIGHT,
        (HUE_BINS * SATURATION_BINS) as u32,
        HASH_BITS,
    ] {
        writer.write_all(&value.to_le_bytes())?;
    }
    for reference in &index.references {
        for view in &reference.views {
            for value in view.full.iter().chain(&view.art) {
                writer.write_all(&value.to_le_bytes())?;
            }
            for value in &view.histogram {
                writer.write_all(&value.to_le_bytes())?;
            }
            writer.write_all(&view.hash.to_le_bytes())?;
        }
    }
    writer.flush()?;
    let mut writer = BufWriter::new(File::create(local_path)?);
    writer.write_all(LOCAL_MAGIC)?;
    writer.write_all(&VERSION.to_le_bytes())?;
    writer.write_all(&count.to_le_bytes())?;
    for reference in &index.references {
        writer.write_all(&(reference.features.len() as u32).to_le_bytes())?;
        for feature in &reference.features {
            writer.write_all(&feature.x.to_le_bytes())?;
            writer.write_all(&feature.y.to_le_bytes())?;
            for bits in feature.bits {
                writer.write_all(&bits.to_le_bytes())?;
            }
        }
    }
    writer.flush()?;
    Ok(())
}

/// Validate the fixed global header, dimensions and exact file length without decoding records.
///
/// Selected and exhaustive readers still validate every descriptor they decode.
/// This structural check is suitable for a low-latency catalogue open.
///
/// # Errors
///
/// Returns an error for an unsupported header, dimension/count mismatch or wrong file size.
pub(super) fn validate_global_layout(path: &Path, count: usize) -> Result<(), Error> {
    let file = File::open(path)?;
    let expected = global_size(count)?;
    if file.metadata()?.len() != expected {
        return Err(invalid("binary global file size mismatch"));
    }
    let mut reader = BufReader::new(file);
    header(&mut reader, GLOBAL_MAGIC, count)?;
    dimensions(&mut reader)
}

/// Validate the local header and safe file-size bounds without decoding feature records.
///
/// The variable-length record boundaries, feature counts and coordinates are
/// checked when a selected or exhaustive reader consumes them.
///
/// # Errors
///
/// Returns an error for an unsupported header or an impossible file size.
pub(super) fn validate_local_layout(path: &Path, count: usize) -> Result<(), Error> {
    let file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let (minimum, maximum) = local_size_bounds(count)?;
    if !(minimum..=maximum).contains(&bytes) {
        return Err(invalid("binary local file size outside descriptor bounds"));
    }
    header(&mut BufReader::new(file), LOCAL_MAGIC, count)
}

/// Read only fixed-width global evidence, borrowing no local features from disk.
///
/// # Errors
///
/// Returns an error for header/dimension/count/size mismatches or invalid vectors.
pub(super) fn globals(path: &Path, cards: &[CardMetadata]) -> Result<CatalogIndex, Error> {
    let file = File::open(path)?;
    let expected = global_size(cards.len())?;
    if file.metadata()?.len() != expected {
        return Err(invalid("binary global file size mismatch"));
    }
    let mut reader = BufReader::new(file);
    header(&mut reader, GLOBAL_MAGIC, cards.len())?;
    dimensions(&mut reader)?;
    let mut references = Vec::with_capacity(cards.len());
    let mut bytes = [0_u8; GLOBAL_BYTES];
    for card in cards {
        reader.read_exact(&mut bytes).map_err(decode_error)?;
        references.push(decode_global(card, &bytes)?);
    }
    eof(&mut reader)?;
    Ok(CatalogIndex { references })
}

/// Read only selected fixed-width global banks from an already validated shard.
///
/// The returned slice retains original offsets so local selection and score
/// lookup cannot be confused by a sparse layout. Unselected references are
/// absent. Each selected descriptor is validated before being returned; fixed
/// structure and length are rechecked on every read.
///
/// # Errors
///
/// Returns an error for out-of-range selections, malformed headers or sizes,
/// invalid selected vectors, failed reads/seeks or trailing bytes.
pub(super) fn globals_selected(
    path: &Path,
    cards: &[CardMetadata],
    offsets: &[usize],
) -> Result<Vec<Option<Reference>>, Error> {
    let mut selected = offsets.to_vec();
    for &offset in &selected {
        if offset >= cards.len() {
            return Err(invalid("global candidate offset outside shard"));
        }
    }
    selected.sort_unstable();
    selected.dedup();
    let file = File::open(path)?;
    let expected = global_size(cards.len())?;
    if file.metadata()?.len() != expected {
        return Err(invalid("binary global file size mismatch"));
    }
    let mut reader = BufReader::new(file);
    header(&mut reader, GLOBAL_MAGIC, cards.len())?;
    dimensions(&mut reader)?;
    let mut references: Vec<Option<Reference>> =
        std::iter::repeat_with(|| None).take(cards.len()).collect();
    let mut bytes = [0_u8; GLOBAL_BYTES];
    let mut next_offset = 0;
    for offset in selected {
        if offset > next_offset {
            reader
                .seek_relative((offset - next_offset) as i64 * GLOBAL_BYTES as i64)
                .map_err(decode_error)?;
        }
        reader.read_exact(&mut bytes).map_err(decode_error)?;
        references[offset] = Some(decode_global(&cards[offset], &bytes)?);
        next_offset = offset + 1;
    }
    if next_offset < cards.len() {
        reader
            .seek_relative((cards.len() - next_offset) as i64 * GLOBAL_BYTES as i64)
            .map_err(decode_error)?;
    }
    if reader.stream_position().map_err(decode_error)? != expected {
        return Err(invalid(
            "binary selected global records do not match file length",
        ));
    }
    eof(&mut reader)?;
    Ok(references)
}

/// Decode and validate one complete global bank in the same way for both readers.
fn decode_global(card: &CardMetadata, bytes: &[u8; GLOBAL_BYTES]) -> Result<Reference, Error> {
    let mut remaining = bytes.as_slice();
    let mut views = Vec::with_capacity(INSETS.len());
    for _ in INSETS {
        let full = signed(&mut remaining, (FULL_WIDTH * FULL_HEIGHT) as usize);
        let art = signed(&mut remaining, (ART_WIDTH * ART_HEIGHT) as usize);
        let histogram = unsigned(&mut remaining, HUE_BINS * SATURATION_BINS);
        let (hash, rest) = remaining.split_at(8);
        let hash = u64::from_le_bytes(hash.try_into().map_err(|_| invalid("invalid hash bytes"))?);
        remaining = rest;
        let mut descriptor = Descriptor {
            full,
            art,
            histogram,
            hash,
            full_norm: 0.0,
            art_norm: 0.0,
            histogram_norm: 0.0,
        };
        descriptor.validate()?;
        views.push(descriptor);
    }
    if views.first().is_none_or(|view| view.full_norm == 0.0) {
        return Err(invalid("binary reference has no luminance structure"));
    }
    Ok(Reference {
        card: card.clone(),
        views,
        features: Vec::new(),
    })
}

/// Populate local features only for validation or exhaustive geometry fallback.
///
/// # Errors
///
/// Returns an error for invalid headers, feature counts, coordinates or file size.
pub(super) fn locals(path: &Path, index: &mut CatalogIndex) -> Result<(), Error> {
    let file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let (minimum, maximum) = local_size_bounds(index.len())?;
    if !(minimum..=maximum).contains(&bytes) {
        return Err(invalid("binary local file size outside descriptor bounds"));
    }
    let mut reader = BufReader::new(file);
    header(&mut reader, LOCAL_MAGIC, index.len())?;
    for reference in &mut index.references {
        let count = read_u32(&mut reader)? as usize;
        if count > MAX_FEATURES {
            return Err(invalid("binary local feature count exceeds bound"));
        }
        reference.features = read_features(&mut reader, count)?;
    }
    eof(&mut reader)
}

/// Read selected local banks while seeking past unselected feature payloads.
///
/// Every record count and the complete file termination are still checked. The
/// coordinates of selected descriptors are validated before being returned.
///
/// # Errors
///
/// Returns an error for malformed headers/counts/sizes, invalid selected
/// coordinates, failed reads/seeks or trailing bytes.
pub(super) fn locals_selected(
    path: &Path,
    references: &mut [Option<Reference>],
) -> Result<(), Error> {
    let offsets = references
        .iter()
        .enumerate()
        .filter_map(|(offset, reference)| reference.as_ref().map(|_| offset))
        .collect::<Vec<_>>();
    let mut selected = local_features_selected(path, references.len(), &offsets)?;
    for (reference, features) in references.iter_mut().zip(&mut selected) {
        if let Some(reference) = reference {
            reference.features = features
                .take()
                .ok_or_else(|| invalid("selected local features missing from sparse result"))?;
        }
    }
    Ok(())
}

/// Read local features only for selected immutable record offsets.
///
/// Record counts and final file termination are checked for the containing
/// shard, while unselected feature payloads are skipped without decoding.
///
/// # Errors
///
/// Returns an error for invalid offsets, malformed headers/counts/sizes,
/// invalid selected coordinates, failed reads/seeks or trailing bytes.
pub(super) fn local_features_selected(
    path: &Path,
    total: usize,
    offsets: &[usize],
) -> Result<Vec<Option<Vec<local::Feature>>>, Error> {
    let mut offsets = offsets.to_vec();
    if offsets.iter().any(|&offset| offset >= total) {
        return Err(invalid("local candidate offset outside shard"));
    }
    offsets.sort_unstable();
    offsets.dedup();
    let file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let (minimum, maximum) = local_size_bounds(total)?;
    if !(minimum..=maximum).contains(&bytes) {
        return Err(invalid("binary local file size outside descriptor bounds"));
    }
    let mut reader = BufReader::new(file);
    header(&mut reader, LOCAL_MAGIC, total)?;
    let mut selected: Vec<Option<Vec<local::Feature>>> =
        std::iter::repeat_with(|| None).take(total).collect();
    let mut next = offsets.into_iter().peekable();
    for (offset, slot) in selected.iter_mut().enumerate() {
        let count = read_u32(&mut reader)? as usize;
        if count > MAX_FEATURES {
            return Err(invalid("binary local feature count exceeds bound"));
        }
        if next.peek() != Some(&offset) {
            reader
                .seek_relative((count * FEATURE_BYTES) as i64)
                .map_err(decode_error)?;
            continue;
        }
        next.next();
        *slot = Some(read_features(&mut reader, count)?);
    }
    if reader.stream_position().map_err(decode_error)? != bytes {
        return Err(invalid(
            "binary selected local records do not match file length",
        ));
    }
    eof(&mut reader)?;
    Ok(selected)
}

/// Compute the exact fixed-width global file size without overflowing.
fn global_size(count: usize) -> Result<u64, Error> {
    let count = u64::try_from(count).map_err(|_| invalid("binary global size overflow"))?;
    GLOBAL_HEADER
        .checked_add(
            count
                .checked_mul(GLOBAL_BYTES as u64)
                .ok_or_else(|| invalid("binary global size overflow"))?,
        )
        .ok_or_else(|| invalid("binary global size overflow"))
}

/// Compute conservative bounds for a variable-length local file without overflowing.
fn local_size_bounds(count: usize) -> Result<(u64, u64), Error> {
    let count = u64::try_from(count).map_err(|_| invalid("binary local size overflow"))?;
    let feature_bytes = (MAX_FEATURES as u64)
        .checked_mul(FEATURE_BYTES as u64)
        .and_then(|bytes| bytes.checked_add(4))
        .ok_or_else(|| invalid("binary local size overflow"))?;
    let minimum = count
        .checked_mul(4)
        .and_then(|bytes| LOCAL_HEADER.checked_add(bytes))
        .ok_or_else(|| invalid("binary local size overflow"))?;
    let maximum = count
        .checked_mul(feature_bytes)
        .and_then(|bytes| LOCAL_HEADER.checked_add(bytes))
        .ok_or_else(|| invalid("binary local size overflow"))?;
    Ok((minimum, maximum))
}

/// Validate the descriptor dimensions stored after the common global header.
fn dimensions(reader: &mut impl Read) -> Result<(), Error> {
    for expected in [
        INSETS.len() as u32,
        FULL_WIDTH * FULL_HEIGHT,
        ART_WIDTH * ART_HEIGHT,
        (HUE_BINS * SATURATION_BINS) as u32,
        HASH_BITS,
    ] {
        if read_u32(reader)? != expected {
            return Err(invalid("binary global descriptor dimensions mismatch"));
        }
    }
    Ok(())
}

/// Decode and validate one selected variable-length local feature bank.
fn read_features(reader: &mut impl Read, count: usize) -> Result<Vec<local::Feature>, Error> {
    let mut features = Vec::with_capacity(count);
    for _ in 0..count {
        let mut bytes = [0_u8; FEATURE_BYTES];
        reader.read_exact(&mut bytes).map_err(decode_error)?;
        let mut bits = [0_u64; 4];
        for (value, chunk) in bits.iter_mut().zip(bytes[4..].as_chunks::<8>().0) {
            *value = u64::from_le_bytes(*chunk);
        }
        features.push(local::Feature {
            x: u16::from_le_bytes([bytes[0], bytes[1]]),
            y: u16::from_le_bytes([bytes[2], bytes[3]]),
            bits,
        });
    }
    local::validate(&features)?;
    Ok(features)
}

/// Decode a fixed-size signed vector from a validated fixed-width record.
fn signed(bytes: &mut &[u8], count: usize) -> Vec<i16> {
    let (values, rest) = bytes.split_at(count * 2);
    *bytes = rest;
    values
        .as_chunks::<2>()
        .0
        .iter()
        .map(|value| i16::from_le_bytes([value[0], value[1]]))
        .collect()
}

/// Decode a fixed-size unsigned histogram from a validated fixed-width record.
fn unsigned(bytes: &mut &[u8], count: usize) -> Vec<u16> {
    let (values, rest) = bytes.split_at(count * 2);
    *bytes = rest;
    values
        .as_chunks::<2>()
        .0
        .iter()
        .map(|value| u16::from_le_bytes([value[0], value[1]]))
        .collect()
}

/// Check representation identity and count before allocating descriptor evidence.
///
/// # Errors
///
/// Returns an error for truncated, unsupported or inconsistent headers.
fn header(reader: &mut impl Read, magic: &[u8; 8], count: usize) -> Result<(), Error> {
    let mut actual = [0_u8; 8];
    reader.read_exact(&mut actual).map_err(decode_error)?;
    if &actual != magic || read_u32(reader)? != VERSION || read_u32(reader)? as usize != count {
        return Err(invalid("unsupported binary shard header or face count"));
    }
    Ok(())
}

/// Read a little-endian header/count integer without accepting truncation.
///
/// # Errors
///
/// Returns an index error when the integer cannot be decoded.
fn read_u32(reader: &mut impl Read) -> Result<u32, Error> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(decode_error)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Require exact termination, rejecting extra records and trailing garbage.
///
/// # Errors
///
/// Returns an error for trailing bytes or failed reads.
fn eof(reader: &mut impl Read) -> Result<(), Error> {
    let mut byte = [0_u8; 1];
    if reader.read(&mut byte).map_err(decode_error)? != 0 {
        return Err(invalid("trailing bytes in binary shard"));
    }
    Ok(())
}

/// Classify malformed binary evidence consistently with malformed JSON indexes.
fn decode_error(error: std::io::Error) -> Error {
    invalid(&format!("could not decode binary shard: {error}"))
}

/// Construct a persisted-data error without exposing a partial index.
fn invalid(message: &str) -> Error {
    Error::InvalidIndex(message.into())
}
