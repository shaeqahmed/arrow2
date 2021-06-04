use parquet2::{
    compression::create_codec,
    encoding::Encoding,
    read::{CompressedPage, PageHeader},
    schema::{CompressionCodec, DataPageHeader},
};

use super::utils;
use crate::{
    array::{Array, BinaryArray, Offset},
    error::Result,
};

pub fn array_to_page_v1<O: Offset>(
    array: &BinaryArray<O>,
    compression: CompressionCodec,
    is_optional: bool,
) -> Result<CompressedPage> {
    let validity = array.validity();

    let mut buffer = utils::write_def_levels(is_optional, validity, array.len())?;

    // append the non-null values
    if is_optional {
        array.iter().for_each(|x| {
            if let Some(x) = x {
                // BYTE_ARRAY: first 4 bytes denote length in littleendian.
                let len = (x.len() as u32).to_le_bytes();
                buffer.extend_from_slice(&len);
                buffer.extend_from_slice(x);
            }
        })
    } else {
        array.values_iter().for_each(|x| {
            // BYTE_ARRAY: first 4 bytes denote length in littleendian.
            let len = (x.len() as u32).to_le_bytes();
            buffer.extend_from_slice(&len);
            buffer.extend_from_slice(x);
        })
    }
    let uncompressed_page_size = buffer.len();

    let codec = create_codec(&compression)?;
    let buffer = if let Some(mut codec) = codec {
        // todo: remove this allocation by extending `buffer` directly.
        // needs refactoring `compress`'s API.
        let mut tmp = vec![];
        codec.compress(&buffer, &mut tmp)?;
        tmp
    } else {
        buffer
    };

    let header = PageHeader::V1(DataPageHeader {
        num_values: array.len() as i32,
        encoding: Encoding::Plain,
        definition_level_encoding: Encoding::Rle,
        repetition_level_encoding: Encoding::Rle,
        statistics: None,
    });

    Ok(CompressedPage::new(
        header,
        buffer,
        compression,
        uncompressed_page_size,
        None,
        None,
    ))
}
