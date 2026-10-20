//! Versioned streaming binary persistence with compressed-payload integrity checks.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::{Compression, bufread::MultiGzDecoder, write::GzEncoder};

use crate::Error;

use super::{Record, TextIndex, position};

/// Payload identity, distinct from portable visual and shard indexes.
const MAGIC: &[u8; 8] = b"WTPTEXT\0";
/// Exact normalization/scoring representation understood by this loader.
const VERSION: u32 = 1;
/// Bound individual strings before allocating a decoded buffer.
const MAX_STRING_BYTES: usize = 16 * 1024 * 1024;

/// Write a staged file and publish it only after gzip and buffered writes succeed.
pub(super) fn save(index: &TextIndex, path: &Path) -> Result<(), Error> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::InvalidOptions("text index destination needs a file name".into()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::InvalidInput(error.to_string()))?
        .as_nanos();
    let temporary = path.with_file_name(format!(
        ".{}.pending-{}-{stamp}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let guard = Pending(temporary.clone());
    let mut writer = GzEncoder::new(BufWriter::new(file), Compression::fast());
    writer.write_all(MAGIC)?;
    write_u32(&mut writer, VERSION)?;
    write_u32(&mut writer, position(index.strings.len())?)?;
    for string in &index.strings {
        if string.len() > MAX_STRING_BYTES {
            return Err(Error::InvalidIndex(
                "text index string exceeds 16 MiB format limit".into(),
            ));
        }
        write_u32(&mut writer, position(string.len())?)?;
        writer.write_all(string.as_bytes())?;
    }
    write_u32(&mut writer, position(index.records.len())?)?;
    for record in &index.records {
        for &value in record
            .metadata
            .iter()
            .chain(&record.titles)
            .chain(&record.rules)
            .chain(&record.footer)
        {
            write_u32(&mut writer, value)?;
        }
    }
    write_ids(&mut writer, &index.titles)?;
    write_u32(&mut writer, position(index.rules.len())?)?;
    for rule in &index.rules {
        write_ids(&mut writer, rule)?;
    }
    write_ids(&mut writer, &index.words)?;
    write_u32(&mut writer, position(index.weights.len())?)?;
    for weight in &index.weights {
        writer.write_all(&weight.to_le_bytes())?;
    }
    let mut writer = writer.finish()?;
    writer.flush()?;
    drop(writer);
    fs::rename(&temporary, path)?;
    drop(guard);
    Ok(())
}

/// Read bounded sections, enforce payload exhaustion/CRC, then validate all references.
pub(super) fn load(path: &Path) -> Result<TextIndex, Error> {
    let mut reader = MultiGzDecoder::new(BufReader::new(File::open(path)?));
    let mut magic = [0_u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC || read_u32(&mut reader)? != VERSION {
        return Err(Error::InvalidIndex(
            "unsupported text index format or version".into(),
        ));
    }
    let count = read_u32(&mut reader)? as usize;
    let mut strings = Vec::new();
    for _ in 0..count {
        let length = read_u32(&mut reader)? as usize;
        if length > MAX_STRING_BYTES {
            return Err(Error::InvalidIndex("oversized text index string".into()));
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes)?;
        strings.push(
            String::from_utf8(bytes)
                .map_err(|_| Error::InvalidIndex("text index string is not UTF-8".into()))?,
        );
    }
    let count = read_u32(&mut reader)? as usize;
    let mut records = Vec::new();
    for _ in 0..count {
        let mut record = Record {
            metadata: [0; 16],
            titles: [0; 2],
            rules: [0; 2],
            footer: [0; 2],
        };
        for value in record
            .metadata
            .iter_mut()
            .chain(&mut record.titles)
            .chain(&mut record.rules)
            .chain(&mut record.footer)
        {
            *value = read_u32(&mut reader)?;
        }
        records.push(record);
    }
    let titles = read_ids(&mut reader, usize::MAX)?;
    let count = read_u32(&mut reader)? as usize;
    let mut rules = Vec::new();
    for _ in 0..count {
        rules.push(read_ids(&mut reader, 256)?);
    }
    let words = read_ids(&mut reader, usize::MAX)?;
    let count = read_u32(&mut reader)? as usize;
    let mut weights = Vec::new();
    for _ in 0..count {
        weights.push(f32::from_bits(read_u32(&mut reader)?));
    }
    if reader.read(&mut [0_u8; 1])? != 0 {
        return Err(Error::InvalidIndex("trailing text index payload".into()));
    }
    TextIndex {
        strings,
        records,
        titles,
        rules,
        words,
        weights,
        title_characters: Vec::new(),
        word_characters: Vec::new(),
        word_ids: HashMap::new(),
        word_rules: Vec::new(),
        title_ids: HashMap::new(),
        title_grams: HashMap::new(),
        title_gram_counts: Vec::new(),
        title_identities: Vec::new(),
        identity_records: Vec::new(),
        footer_sets: HashMap::new(),
        footer_numbers: HashMap::new(),
        footer_records: HashMap::new(),
    }
    .prepare()
}

/// Serialize a count and a contiguous sequence of 32-bit IDs.
fn write_ids(writer: &mut impl Write, values: &[u32]) -> Result<(), Error> {
    write_u32(writer, position(values.len())?)?;
    for &value in values {
        write_u32(writer, value)?;
    }
    Ok(())
}

/// Read incrementally so a corrupt count alone cannot reserve enormous arrays.
fn read_ids(reader: &mut impl Read, maximum: usize) -> Result<Vec<u32>, Error> {
    let count = read_u32(reader)? as usize;
    if count > maximum {
        return Err(Error::InvalidIndex(
            "oversized text index token sequence".into(),
        ));
    }
    let mut values = Vec::new();
    for _ in 0..count {
        values.push(read_u32(reader)?);
    }
    Ok(values)
}

/// Write the format's explicit little-endian integer representation.
fn write_u32(writer: &mut impl Write, value: u32) -> Result<(), Error> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

/// Read one complete little-endian integer or fail on truncated data.
fn read_u32(reader: &mut impl Read) -> Result<u32, Error> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Remove an abandoned staged file on every failure path.
struct Pending(PathBuf);

impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
