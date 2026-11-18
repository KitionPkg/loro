use std::io::{self, Write};

use bytes::Bytes;
use loro_common::LoroError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    None,
    LZ4,
}

impl CompressionType {
    pub fn is_none(&self) -> bool {
        matches!(self, CompressionType::None)
    }
}

impl TryFrom<u8> for CompressionType {
    type Error = LoroError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(CompressionType::None),
            1 => Ok(CompressionType::LZ4),
            _ => Err(LoroError::DecodeError(
                format!("Invalid compression type: {value}").into(),
            )),
        }
    }
}

impl From<CompressionType> for u8 {
    fn from(value: CompressionType) -> Self {
        match value {
            CompressionType::None => 0,
            CompressionType::LZ4 => 1,
        }
    }
}

pub fn compress(w: &mut Vec<u8>, data: &[u8], compression_type: CompressionType) {
    match compression_type {
        CompressionType::None => {
            w.write_all(data).unwrap();
        }
        CompressionType::LZ4 => {
            let mut encoder = lz4_flex::frame::FrameEncoder::new(w);
            encoder.write_all(data).unwrap();
            let _w = encoder.finish().unwrap();
        }
    }
}

pub fn decompress(
    out: &mut Vec<u8>,
    data: Bytes,
    compression_type: CompressionType,
) -> Result<(), LoroError> {
    match compression_type {
        CompressionType::None => {
            out.write_all(&data).unwrap();
            Ok(())
        }
        CompressionType::LZ4 => {
            let start = out.len();
            if decompress_lz4_frame(out, &data).is_some() {
                return Ok(());
            }
            out.truncate(start);
            let mut decoder = lz4_flex::frame::FrameDecoder::new(data.as_ref());
            io::copy(&mut decoder, out)
                .map_err(|e| LoroError::DecodeError(e.to_string().into()))?;
            Ok(())
        }
    }
}

const LZ4_FRAME_MAGIC: u32 = 0x184D_2204;
const FLG_VERSION_MASK: u8 = 0b1100_0000;
const FLG_VERSION: u8 = 0b0100_0000;
const FLG_INDEPENDENT_BLOCKS: u8 = 0b0010_0000;
const FLG_BLOCK_CHECKSUMS: u8 = 0b0001_0000;
const FLG_CONTENT_SIZE: u8 = 0b0000_1000;
const FLG_CONTENT_CHECKSUM: u8 = 0b0000_0100;
const FLG_RESERVED: u8 = 0b0000_0010;
const FLG_DICTIONARY_ID: u8 = 0b0000_0001;
const BLOCK_UNCOMPRESSED: u32 = 0x8000_0000;

/// Decodes one LZ4 frame that fills `data`, as [`compress`] writes it (no checksums but the
/// header's, no dictionary), appending its content to `out`. Each block is decompressed into
/// about the bytes it holds: `lz4_flex`'s frame decoder allocates and zeroes buffers of the
/// frame's largest block size (64 KiB and more) for every frame, which costs more than the
/// decompression itself for the KV store's blocks of a few KiB.
///
/// `None` for anything else (another frame shape, several frames, or malformed input): the
/// caller then truncates `out` and decodes with the frame decoder, which reports the error.
fn decompress_lz4_frame(out: &mut Vec<u8>, data: &[u8]) -> Option<()> {
    let magic = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    let (flg, bd) = (*data.get(4)?, *data.get(5)?);
    if magic != LZ4_FRAME_MAGIC
        || flg & FLG_VERSION_MASK != FLG_VERSION
        || flg & (FLG_RESERVED | FLG_DICTIONARY_ID | FLG_BLOCK_CHECKSUMS | FLG_CONTENT_CHECKSUM)
            != 0
        || bd & 0b1000_1111 != 0
    {
        return None;
    }
    let max_block_size = match (bd >> 4) & 0b111 {
        4 => 64 << 10,
        5 => 256 << 10,
        6 => 1 << 20,
        7 => 4 << 20,
        _ => return None,
    };
    let mut at = 6;
    let content_size = if flg & FLG_CONTENT_SIZE != 0 {
        let size = u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        Some(size)
    } else {
        None
    };
    let header_checksum = (xxhash_rust::xxh32::xxh32(&data[4..at], 0) >> 8) as u8;
    if *data.get(at)? != header_checksum {
        return None;
    }
    at += 1;
    let linked = flg & FLG_INDEPENDENT_BLOCKS == 0;
    let start = out.len();
    let mut blocks = 0;
    loop {
        let info = u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        if info == 0 {
            break;
        }
        let len = (info & !BLOCK_UNCOMPRESSED) as usize;
        if len > max_block_size {
            return None;
        }
        let block = data.get(at..at.checked_add(len)?)?;
        at += len;
        if info & BLOCK_UNCOMPRESSED != 0 {
            out.extend_from_slice(block);
        } else {
            // A linked block after the first reads the blocks before it; the frame decoder does that.
            if linked && blocks > 0 {
                return None;
            }
            // Most blocks decompress to at most four times their size: into that, else into the
            // size their sequences add up to.
            let from = out.len();
            let guess = len.saturating_mul(4).saturating_add(64).min(max_block_size);
            out.resize(from + guess, 0);
            match lz4_flex::block::decompress_into(block, &mut out[from..]) {
                Ok(written) => out.truncate(from + written),
                Err(_) => {
                    let size = lz4_block_output_len(block)?;
                    if size <= guess || size > max_block_size {
                        return None;
                    }
                    out.resize(from + size, 0);
                    let written = lz4_flex::block::decompress_into(block, &mut out[from..]).ok()?;
                    if written != size {
                        return None;
                    }
                }
            }
        }
        blocks += 1;
    }
    if at != data.len() || content_size.is_some_and(|size| size != (out.len() - start) as u64) {
        return None;
    }
    Some(())
}

/// The number of bytes an LZ4 block decompresses to, read from its sequences' lengths without
/// copying anything; `None` when the block ends inside a sequence.
fn lz4_block_output_len(block: &[u8]) -> Option<usize> {
    let extended = |at: &mut usize, mut len: usize| -> Option<usize> {
        loop {
            let byte = *block.get(*at)?;
            *at += 1;
            len = len.checked_add(byte as usize)?;
            if byte != u8::MAX {
                return Some(len);
            }
        }
    };
    let (mut at, mut size) = (0usize, 0usize);
    loop {
        let token = *block.get(at)?;
        at += 1;
        let mut literals = (token >> 4) as usize;
        if literals == 15 {
            literals = extended(&mut at, literals)?;
        }
        at = at.checked_add(literals)?;
        size = size.checked_add(literals)?;
        if at == block.len() {
            return Some(size);
        }
        // The match offset, then its length.
        at = at.checked_add(2)?;
        if at > block.len() {
            return None;
        }
        let mut matched = (token & 0b1111) as usize;
        if matched == 15 {
            matched = extended(&mut at, matched)?;
        }
        size = size.checked_add(matched + 4)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_decoder(data: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        io::copy(&mut lz4_flex::frame::FrameDecoder::new(data), &mut out)
            .map_err(|e| e.to_string())?;
        Ok(out)
    }

    fn samples() -> Vec<Vec<u8>> {
        let mut state = 0x2545_f491_u32;
        let mut noise = |len: usize| -> Vec<u8> {
            (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect()
        };
        let text = |len: usize| -> Vec<u8> {
            b"row 17 words 3, number_1 = 170; "
                .iter()
                .copied()
                .cycle()
                .take(len)
                .collect()
        };
        vec![
            Vec::new(),
            vec![7],
            text(15),
            text(4096),
            noise(4096),
            [text(3000), noise(1000), vec![0; 300]].concat(),
            vec![0; 60 << 10],
            text(64 << 10),
            text((64 << 10) + 1),
            noise(300 << 10),
            text(5 << 20),
        ]
    }

    #[test]
    fn lz4_frames_decode_as_the_frame_decoder_decodes_them() {
        for data in samples() {
            let mut frame = Vec::new();
            compress(&mut frame, &data, CompressionType::LZ4);
            let mut fast = vec![1, 2, 3];
            assert!(
                decompress_lz4_frame(&mut fast, &frame).is_some(),
                "{} bytes",
                data.len()
            );
            assert_eq!(&fast[3..], &data[..], "{} bytes", data.len());
            assert_eq!(frame_decoder(&frame).unwrap(), data);
            let mut out = vec![9];
            decompress(&mut out, Bytes::from(frame), CompressionType::LZ4).unwrap();
            assert_eq!(&out[1..], &data[..]);
        }
    }

    #[test]
    fn lz4_block_lengths_match_the_decompressed_size() {
        for data in samples() {
            let block = lz4_flex::block::compress(&data);
            assert_eq!(lz4_block_output_len(&block), Some(data.len()));
            if !block.is_empty() {
                assert_eq!(lz4_block_output_len(&block[..block.len() - 1]), None);
            }
        }
    }

    #[test]
    fn malformed_lz4_frames_fail_as_before() {
        for data in samples() {
            let mut frame = Vec::new();
            compress(&mut frame, &data, CompressionType::LZ4);
            let cuts = [1, 5, 7, frame.len() / 2, frame.len().saturating_sub(1)];
            for cut in cuts.into_iter().filter(|cut| *cut < frame.len()) {
                let broken = &frame[..cut];
                let mut out = vec![5];
                let ours = decompress(
                    &mut out,
                    Bytes::copy_from_slice(broken),
                    CompressionType::LZ4,
                );
                match frame_decoder(broken) {
                    Ok(decoded) => assert_eq!(&out[1..], &decoded[..]),
                    Err(error) => assert_eq!(
                        ours.unwrap_err().to_string(),
                        LoroError::DecodeError(error.into()).to_string()
                    ),
                }
            }
            // A corrupted header checksum is refused by both.
            if frame.len() > 6 {
                let mut broken = frame.clone();
                broken[6] ^= 0x55;
                let mut out = Vec::new();
                assert!(decompress_lz4_frame(&mut out, &broken).is_none());
                assert!(decompress(&mut out, Bytes::from(broken), CompressionType::LZ4).is_err());
            }
        }
    }

    #[test]
    fn bytes_after_the_frame_decode_through_the_frame_decoder() {
        let (a, b) = (b"first frame".repeat(40), b"second".repeat(9));
        let mut frames = Vec::new();
        compress(&mut frames, &a, CompressionType::LZ4);
        compress(&mut frames, &b, CompressionType::LZ4);
        let mut out = Vec::new();
        assert!(decompress_lz4_frame(&mut out, &frames).is_none());
        let mut out = Vec::new();
        decompress(&mut out, Bytes::from(frames.clone()), CompressionType::LZ4).unwrap();
        assert_eq!(out, frame_decoder(&frames).unwrap());
    }
}
