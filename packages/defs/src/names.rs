use crate::bytes::{TakeError, TakeNullTerminatedUtf8, take, take_null_terminated_utf8};
use crate::bytes::{put, put_bytes, put_null_terminated_utf8};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Read},
    path::Path,
};

#[derive(Debug)]
pub struct Names {
    pub header_bytes: [u8; 20],
    pub map: BTreeMap<u32, NamesEntry>,
}

#[derive(Debug)]
pub enum NamesError {
    Io(io::Error),
    UnexpectedEnd,
    ParseEntry(usize, NamesEntryError),
    /// Failed to extract 20-byte header.
    MissingHeader,
}

impl From<io::Error> for NamesError {
    fn from(e: io::Error) -> Self { NamesError::Io(e) }
}

#[derive(Debug)]
pub enum NamesEntryError {
    Crc(TakeError),
    String(TakeNullTerminatedUtf8),
}

impl Names {
    pub fn load(path: &Path) -> Result<Self, NamesError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        Self::from_reader(reader)
    }
}

impl Names {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self, NamesError> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        Self::from_bytes(&buf)
    }
}

impl Names {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, NamesError> {
        let bytes_cursor = &mut &bytes[..];

        let header_bytes: [u8; 20] = bytes_cursor
            .get(..20)
            .ok_or(NamesError::MissingHeader)?
            .try_into()
            .unwrap();
        *bytes_cursor = &bytes_cursor[20..];

        let mut map = BTreeMap::new();

        while !bytes_cursor.is_empty() {
            let offset = bytes.len() - bytes_cursor.len();

            let entry =
                NamesEntry::parse(bytes_cursor)
                    .map_err(|error| NamesError::ParseEntry(offset, error))?;

            let string_offset = (offset + 4 - 20) as u32;

            map.insert(string_offset, entry);
        }

        Ok(Self { header_bytes, map })
    }

    /// Serialize to the wire format (header + entries).
    pub fn to_bytes(&self) -> Vec<u8> {
        let total: usize = 20 + self.map.values().map(|e| e.byte_size()).sum::<usize>();
        let mut out = vec![0u8; total];
        let mut cur: &mut [u8] = &mut out;
        put_bytes(&mut cur, &self.header_bytes).unwrap();
        for entry in self.map.values() {
            entry.serialize(&mut cur).unwrap();
        }
        debug_assert!(cur.is_empty(), "Names::to_bytes: buffer over/underflow");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_names_bin() {
        // Load the real OG retail names.bin and verify byte-exact roundtrip.
        let path = Path::new(env!("HOME")).join("Fable/data/CompiledDefs/names.bin");
        if !path.exists() {
            return; // skip if data not available
        }
        let original = std::fs::read(&path).unwrap();
        let names = Names::from_bytes(&original).unwrap();
        let re_serialized = names.to_bytes();
        assert_eq!(re_serialized.len(), original.len(), "length mismatch");
        assert_eq!(re_serialized, original, "byte-exact roundtrip failed");
    }
}

#[derive(Debug, Clone)]
pub struct NamesEntry {
    pub crc: u32,
    pub string: String,
}

impl NamesEntry {
    fn parse(input: &mut &[u8]) -> Result<Self, NamesEntryError> {
        let crc = take::<u32>(input).map_err(NamesEntryError::Crc)?.to_le();
        let string = take_null_terminated_utf8(input)
            .map_err(NamesEntryError::String)?
            .to_owned();
        Ok(Self { crc, string })
    }

    fn byte_size(&self) -> usize {
        4 + self.string.len() + 1
    }

    fn serialize(&self, out: &mut &mut [u8]) -> Result<(), crate::bytes::UnexpectedEnd> {
        put(out, &self.crc)?;
        put_null_terminated_utf8(out, &self.string)?;
        Ok(())
    }
}
