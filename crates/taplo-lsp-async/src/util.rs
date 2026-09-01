//! Checked conversions between Rowan byte offsets and LSP source positions.

use std::iter::Peekable;
use std::mem;
use std::str::CharIndices;

use lsp_types::Position;
use lsp_types::Range;
use rowan::TextRange;
use rowan::TextSize;
use thiserror::Error;

/// The character-unit encoding used for LSP position columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionEncoding {
  /// Count UTF-8 code units, which are source bytes.
  Utf8,
  /// Count UTF-16 code units, as required by the original LSP position model.
  Utf16,
  /// Count Unicode scalar values.
  Utf32,
}

/// A failed conversion between a source offset and an LSP position.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MappingError {
  /// The source cannot be represented by Rowan's `u32` byte offsets.
  #[error("source length {length} exceeds the maximum supported byte offset")]
  SourceTooLarge {
    /// The source length in bytes.
    length: usize,
  },
  /// The source contains more lines than an LSP position can represent.
  #[error("source line {line} exceeds the maximum supported LSP line")]
  LineOverflow {
    /// The zero-based source line that could not be represented.
    line: usize,
  },
  /// A character column overflowed the LSP `u32` coordinate.
  #[error("character column on line {line} exceeds the maximum supported LSP character")]
  CharacterOverflow {
    /// The zero-based source line containing the overflowing column.
    line: u32,
  },
  /// A byte offset lies beyond the source.
  #[error("byte offset {offset} lies beyond source length {source_length}")]
  OffsetOutOfBounds {
    /// The requested byte offset.
    offset:        u32,
    /// The source length in bytes.
    source_length: u32,
  },
  /// A byte offset is not a representable source-character boundary.
  #[error("byte offset {offset} is not a representable character boundary")]
  OffsetNotBoundary {
    /// The requested byte offset.
    offset: u32,
  },
  /// An LSP line lies beyond the source.
  #[error("position line {line} lies beyond the source")]
  PositionLineOutOfBounds {
    /// The requested zero-based line.
    line: u32,
  },
  /// An LSP column lies beyond its source line.
  #[error("position character {character} lies beyond line {line}")]
  PositionCharacterOutOfBounds {
    /// The requested zero-based line.
    line:      u32,
    /// The requested character column.
    character: u32,
  },
  /// An LSP column splits an encoded source character.
  #[error("position character {character} on line {line} is not a character boundary")]
  PositionNotBoundary {
    /// The requested zero-based line.
    line:      u32,
    /// The requested character column.
    character: u32,
  },
  /// A range ends before it starts.
  #[error("range end {end_line}:{end_character} precedes start {start_line}:{start_character}")]
  ReversedRange {
    /// The range's start line.
    start_line:      u32,
    /// The range's start character.
    start_character: u32,
    /// The range's end line.
    end_line:        u32,
    /// The range's end character.
    end_character:   u32,
  },
  /// A relative position precedes its reference position.
  #[error("position {line}:{character} precedes reference position {reference_line}:{reference_character}")]
  RelativePositionUnderflow {
    /// The absolute position line.
    line:                u32,
    /// The absolute position character.
    character:           u32,
    /// The reference position line.
    reference_line:      u32,
    /// The reference position character.
    reference_character: u32,
  },
}

/// The columns represented by one source-character boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Checkpoint {
  /// The absolute source byte offset.
  offset: TextSize,
  /// The UTF-8 column at this boundary.
  utf8:   u32,
  /// The UTF-16 column at this boundary.
  utf16:  u32,
  /// The UTF-32 column at this boundary.
  utf32:  u32,
}

impl Checkpoint {
  /// Construct the zero-column checkpoint for one source line.
  const fn line_start(offset: TextSize) -> Self {
    Self {
      offset,
      utf8: 0,
      utf16: 0,
      utf32: 0,
    }
  }

  /// Return the column for the selected position encoding.
  const fn column(self, encoding: PositionEncoding) -> u32 {
    match encoding {
      PositionEncoding::Utf8 => self.utf8,
      PositionEncoding::Utf16 => self.utf16,
      PositionEncoding::Utf32 => self.utf32,
    }
  }

  /// Advance every supported position encoding across one source character.
  fn after(self, offset: TextSize, character: char, line: u32) -> Result<Self, MappingError> {
    let utf8_width = u32::try_from(character.len_utf8()).map_err(|_conversion_error| MappingError::CharacterOverflow {
      line,
    })?;
    let utf16_width = u32::try_from(character.len_utf16()).map_err(|_conversion_error| MappingError::CharacterOverflow {
      line,
    })?;
    Ok(Self {
      offset,
      utf8: self.utf8.checked_add(utf8_width).ok_or(MappingError::CharacterOverflow {
        line,
      })?,
      utf16: self.utf16.checked_add(utf16_width).ok_or(MappingError::CharacterOverflow {
        line,
      })?,
      utf32: self.utf32.checked_add(1).ok_or(MappingError::CharacterOverflow {
        line,
      })?,
    })
  }
}

/// The checked coordinate data for one source line.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Line {
  /// The zero-based LSP line number.
  number:         u32,
  /// The absolute byte offset at which the line starts.
  start:          TextSize,
  /// The byte offset immediately after the line's textual content.
  content_end:    TextSize,
  /// The byte offset immediately after the line terminator.
  terminator_end: TextSize,
  /// All valid character boundaries within the textual content.
  checkpoints:    Vec<Checkpoint>,
}

/// Stateful owner of line and character-boundary construction invariants.
struct MapperBuilder {
  /// Complete source length in bytes for typed conversion diagnostics.
  source_byte_length: usize,
  /// Complete source length in Rowan's coordinate type.
  source_extent:      TextSize,
  /// Completed source lines.
  lines:              Vec<Line>,
  /// Byte offset at which the line under construction starts.
  line_start:         usize,
  /// Character boundaries for the line under construction.
  checkpoints:        Vec<Checkpoint>,
  /// Most recently recorded character boundary.
  current:            Checkpoint,
}

impl MapperBuilder {
  /// Start mapping one source after validating its complete byte length.
  #[allow(
    clippy::single_call_fn,
    reason = "the builder constructor centralizes source-extent validation before incremental line mapping"
  )]
  fn new(source_byte_length: usize) -> Result<Self, MappingError> {
    let source_extent = text_size(source_byte_length, source_byte_length)?;
    let current = Checkpoint::line_start(TextSize::from(0));
    Ok(Self {
      source_byte_length,
      source_extent,
      lines: Vec::new(),
      line_start: 0,
      checkpoints: vec![current],
      current,
    })
  }

  /// Record one ordinary source character and all of its encoded column widths.
  fn push_character(&mut self, offset: usize, character: char) -> Result<(), MappingError> {
    let character_end = character_end(offset, character, self.source_byte_length)?;
    let line = current_line_number(self.lines.len())?;
    self.current = self
      .current
      .after(text_size(character_end, self.source_byte_length)?, character, line)?;
    self.checkpoints.push(self.current);
    Ok(())
  }

  /// Complete the current source line and initialize the next line boundary.
  fn finish_line(&mut self, content_end: usize, terminator_end: usize) -> Result<(), MappingError> {
    let number = current_line_number(self.lines.len())?;
    self.lines.push(Line {
      number,
      start: text_size(self.line_start, self.source_byte_length)?,
      content_end: text_size(content_end, self.source_byte_length)?,
      terminator_end: text_size(terminator_end, self.source_byte_length)?,
      checkpoints: mem::take(&mut self.checkpoints),
    });
    self.line_start = terminator_end;
    self.current = Checkpoint::line_start(text_size(terminator_end, self.source_byte_length)?);
    self.checkpoints.push(self.current);
    Ok(())
  }

  /// Complete the final line and produce the immutable mapper.
  fn finish(mut self, encoding: PositionEncoding) -> Result<Mapper, MappingError> {
    let end = Position::new(current_line_number(self.lines.len())?, self.current.column(encoding));
    self.finish_line(self.source_byte_length, self.source_byte_length)?;
    Ok(Mapper {
      lines: self.lines,
      encoding,
      source_length: self.source_extent,
      end,
    })
  }
}

/// A checked, compact mapper between Rowan byte offsets and LSP positions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mapper {
  /// The source's line and character-boundary tables.
  lines:         Vec<Line>,
  /// The selected LSP position encoding.
  encoding:      PositionEncoding,
  /// The source length as a Rowan-compatible byte offset.
  source_length: TextSize,
  /// The position immediately after the complete source.
  end:           Position,
}

impl Mapper {
  /// Build a mapper whose character columns count UTF-8 code units.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] if the source length or a resulting coordinate cannot be represented
  /// by Rowan and LSP's `u32` coordinate types.
  pub fn new_utf8(source: &str) -> Result<Self, MappingError> {
    Self::new(source, PositionEncoding::Utf8)
  }

  /// Build a mapper whose character columns count UTF-16 code units.
  ///
  /// ```
  /// use lsp_types::Position;
  /// use rowan::TextSize;
  /// use strict_test_support::TestFailure;
  /// use strict_test_support::ensure;
  /// use strict_test_support::ensure_ok;
  /// use taplo_lsp_async::util::Mapper;
  ///
  /// fn main() -> Result<(), TestFailure> {
  ///   let mapper = ensure_ok(
  ///     Mapper::new_utf16("a\u{1f600}z"),
  ///     "the UTF-16 mapper must construct",
  ///   )?;
  ///   let position = ensure_ok(
  ///     mapper.position(TextSize::from(5)),
  ///     "the emoji boundary must map",
  ///   )?;
  ///   ensure(
  ///     position == Position::new(0, 3),
  ///     "UTF-16 columns must count surrogate code units",
  ///   )
  /// }
  /// ```
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] if the source length or a resulting coordinate cannot be represented
  /// by Rowan and LSP's `u32` coordinate types.
  pub fn new_utf16(source: &str) -> Result<Self, MappingError> {
    Self::new(source, PositionEncoding::Utf16)
  }

  /// Build a mapper whose character columns count Unicode scalar values.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] if the source length or a resulting coordinate cannot be represented
  /// by Rowan and LSP's `u32` coordinate types.
  pub fn new_utf32(source: &str) -> Result<Self, MappingError> {
    Self::new(source, PositionEncoding::Utf32)
  }

  /// Build a mapper using the requested position encoding.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] if the source length or a resulting coordinate cannot be represented
  /// by Rowan and LSP's `u32` coordinate types.
  pub fn new(source: &str, encoding: PositionEncoding) -> Result<Self, MappingError> {
    let mut builder = MapperBuilder::new(source.len())?;
    let mut characters = source.char_indices().peekable();

    while let Some((offset, character)) = characters.next() {
      if let Some(terminator_end) = line_terminator_end(&mut characters, offset, character, source.len())? {
        builder.finish_line(offset, terminator_end)?;
      } else {
        builder.push_character(offset, character)?;
      }
    }

    builder.finish(encoding)
  }

  /// Convert an LSP position to its Rowan byte offset.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] when the line or column is outside the source or the requested column
  /// splits a source character in the selected encoding.
  pub fn offset(&self, position: Position) -> Result<TextSize, MappingError> {
    let line_index = usize::try_from(position.line).map_err(|_conversion_error| MappingError::PositionLineOutOfBounds {
      line: position.line,
    })?;
    let line = self.lines.get(line_index).ok_or(MappingError::PositionLineOutOfBounds {
      line: position.line
    })?;
    let Some(last_checkpoint) = line.checkpoints.last().copied() else {
      return Err(MappingError::PositionCharacterOutOfBounds {
        line:      position.line,
        character: position.character,
      });
    };
    let maximum = last_checkpoint.column(self.encoding);
    if position.character > maximum {
      return Err(MappingError::PositionCharacterOutOfBounds {
        line:      position.line,
        character: position.character,
      });
    }

    let checkpoint_index = line
      .checkpoints
      .binary_search_by_key(&position.character, |checkpoint| checkpoint.column(self.encoding))
      .map_err(|_binary_search_error| MappingError::PositionNotBoundary {
        line:      position.line,
        character: position.character,
      })?;
    line
      .checkpoints
      .get(checkpoint_index)
      .map(|checkpoint| checkpoint.offset)
      .ok_or(MappingError::PositionNotBoundary {
        line:      position.line,
        character: position.character,
      })
  }
  /// Convert an LSP range to its Rowan byte range.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] if the range is reversed or either endpoint is invalid.
  pub fn text_range(&self, range: Range) -> Result<TextRange, MappingError> {
    ensure_ordered_range(range)?;
    let start = self.offset(range.start)?;
    let end = self.offset(range.end)?;
    Ok(TextRange::new(start, end))
  }

  /// Convert a Rowan byte offset to its LSP position.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] when the offset lies outside the source, inside a multibyte source
  /// character, or within a multi-byte line terminator.
  pub fn position(&self, offset: TextSize) -> Result<Position, MappingError> {
    if offset > self.source_length {
      return Err(MappingError::OffsetOutOfBounds {
        offset:        u32::from(offset),
        source_length: u32::from(self.source_length),
      });
    }
    let line_index = self.lines.partition_point(|line| line.start <= offset).saturating_sub(1);
    let Some(line) = self.lines.get(line_index) else {
      return Err(MappingError::OffsetOutOfBounds {
        offset:        u32::from(offset),
        source_length: u32::from(self.source_length),
      });
    };
    if offset > line.content_end {
      return Err(MappingError::OffsetNotBoundary {
        offset: u32::from(offset)
      });
    }

    let checkpoint_index = line
      .checkpoints
      .binary_search_by_key(&offset, |checkpoint| checkpoint.offset)
      .map_err(|_binary_search_error| MappingError::OffsetNotBoundary {
        offset: u32::from(offset)
      })?;
    line
      .checkpoints
      .get(checkpoint_index)
      .map(|checkpoint| Position::new(line.number, checkpoint.column(self.encoding)))
      .ok_or_else(|| MappingError::OffsetNotBoundary {
        offset: u32::from(offset)
      })
  }

  /// Convert a Rowan byte range to its LSP range.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] when either endpoint is outside the source or not representable in
  /// the selected LSP position encoding.
  pub fn range(&self, range: TextRange) -> Result<Range, MappingError> {
    Ok(Range::new(self.position(range.start())?, self.position(range.end())?))
  }

  /// Return the number of source lines represented by this mapper.
  #[must_use]
  pub const fn line_count(&self) -> usize {
    self.lines.len()
  }

  /// Return the LSP range covering the complete source.
  #[must_use]
  pub fn all_range(&self) -> Range {
    Range {
      start: Position::new(0, 0),
      end:   self.end,
    }
  }

  /// Return the mapper's selected position encoding.
  #[must_use]
  pub const fn encoding(&self) -> PositionEncoding {
    self.encoding
  }
}

/// Convert an absolute position into the delta from a preceding position.
///
/// # Errors
///
/// Returns [`MappingError::RelativePositionUnderflow`] when `position` precedes `reference`.
pub fn relative_position(position: Position, reference: Position) -> Result<Position, MappingError> {
  if position.line < reference.line || (position.line == reference.line && position.character < reference.character) {
    return Err(MappingError::RelativePositionUnderflow {
      line:                position.line,
      character:           position.character,
      reference_line:      reference.line,
      reference_character: reference.character,
    });
  }

  if position.line == reference.line {
    return Ok(Position::new(
      0,
      position
        .character
        .checked_sub(reference.character)
        .ok_or(MappingError::RelativePositionUnderflow {
          line:                position.line,
          character:           position.character,
          reference_line:      reference.line,
          reference_character: reference.character,
        })?,
    ));
  }

  Ok(Position::new(
    position
      .line
      .checked_sub(reference.line)
      .ok_or(MappingError::RelativePositionUnderflow {
        line:                position.line,
        character:           position.character,
        reference_line:      reference.line,
        reference_character: reference.character,
      })?,
    position.character,
  ))
}

/// Convert an absolute range into the start-to-start delta from a preceding range.
///
/// # Errors
///
/// Returns [`MappingError`] when either range is reversed or the new range starts before the
/// reference range.
pub fn relative_range(range: Range, reference: Range) -> Result<Range, MappingError> {
  ensure_ordered_range(range)?;
  ensure_ordered_range(reference)?;
  Ok(Range::new(
    relative_position(range.start, reference.start)?,
    relative_position(range.end, reference.start)?,
  ))
}

/// Return the checked byte offset after a complete source line terminator.
#[allow(
  clippy::single_call_fn,
  reason = "line-terminator classification owns CRLF lookahead and checked byte-end calculation"
)]
fn line_terminator_end(
  characters: &mut Peekable<CharIndices<'_>>,
  offset: usize,
  character: char,
  source_length: usize,
) -> Result<Option<usize>, MappingError> {
  match character {
    '\r' => match characters.next_if(|&(_, next_character)| next_character == '\n') {
      Some((next_offset, next_character)) => character_end(next_offset, next_character, source_length).map(Some),
      None => character_end(offset, character, source_length).map(Some),
    },
    '\n' => character_end(offset, character, source_length).map(Some),
    _ => Ok(None),
  }
}

/// Return the checked byte offset immediately after one source character.
const fn character_end(offset: usize, character: char, source_length: usize) -> Result<usize, MappingError> {
  match offset.checked_add(character.len_utf8()) {
    Some(end) => Ok(end),
    None => Err(MappingError::SourceTooLarge {
      length: source_length
    }),
  }
}

/// Convert a line vector length into its LSP line number.
fn current_line_number(line: usize) -> Result<u32, MappingError> {
  u32::try_from(line).map_err(|_conversion_error| MappingError::LineOverflow {
    line,
  })
}

/// Convert a checked source byte index into a Rowan offset.
fn text_size(offset: usize, source_length: usize) -> Result<TextSize, MappingError> {
  let raw = u32::try_from(offset).map_err(|_conversion_error| MappingError::SourceTooLarge {
    length: source_length
  })?;
  Ok(TextSize::from(raw))
}

/// Validate that an LSP range is ordered.
const fn ensure_ordered_range(range: Range) -> Result<(), MappingError> {
  if range.end.line < range.start.line || (range.end.line == range.start.line && range.end.character < range.start.character) {
    return Err(MappingError::ReversedRange {
      start_line:      range.start.line,
      start_character: range.start.character,
      end_line:        range.end.line,
      end_character:   range.end.character,
    });
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use core::iter::once;

  use lsp_types::Position;
  use lsp_types::Range;
  use proptest::arbitrary::any;
  use proptest::strict::ensure_property;
  use rowan::TextRange;
  use rowan::TextSize;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::Mapper;
  use super::MappingError;
  use super::relative_position;
  use super::relative_range;

  /// Assert UTF-8, UTF-16, and UTF-32 columns at Unicode boundaries.
  #[test]
  fn maps_each_position_encoding_at_character_boundaries() -> Result<(), TestFailure> {
    let source = "a\u{1f600}z";
    let utf8 = ensure_ok(Mapper::new_utf8(source), "UTF-8 mapper construction")?;
    let utf16 = ensure_ok(Mapper::new_utf16(source), "UTF-16 mapper construction")?;
    let utf32 = ensure_ok(Mapper::new_utf32(source), "UTF-32 mapper construction")?;
    let after_emoji = TextSize::from(5);

    let utf8_position = ensure_ok(utf8.position(after_emoji), "the UTF-8 emoji boundary must map")?;
    ensure(utf8_position == Position::new(0, 5), "UTF-8 columns must count source bytes")?;
    let utf16_position = ensure_ok(utf16.position(after_emoji), "the UTF-16 emoji boundary must map")?;
    ensure(
      utf16_position == Position::new(0, 3),
      "UTF-16 columns must count surrogate code units",
    )?;
    let utf32_position = ensure_ok(utf32.position(after_emoji), "the UTF-32 emoji boundary must map")?;
    ensure(
      utf32_position == Position::new(0, 2),
      "UTF-32 columns must count Unicode scalar values",
    )?;
    let split_multibyte = ensure_some(
      utf8.position(TextSize::from(2)).err(),
      "a UTF-8 byte offset within a multibyte character must be rejected",
    )?;
    ensure(
      split_multibyte
        == MappingError::OffsetNotBoundary {
          offset: 2
        },
      "the multibyte-character failure must retain its exact byte offset",
    )?;
    let split_surrogate = ensure_some(
      utf16.offset(Position::new(0, 2)).err(),
      "a UTF-16 column within a surrogate pair must be rejected",
    )?;
    ensure(
      split_surrogate
        == MappingError::PositionNotBoundary {
          line:      0,
          character: 2,
        },
      "the surrogate-pair failure must retain its exact position",
    )
  }

  /// Assert LF, CRLF, empty lines, and a final empty line.
  #[test]
  fn maps_line_terminators_without_exposing_crlf_interior() -> Result<(), TestFailure> {
    let mapper = ensure_ok(Mapper::new_utf16("a\r\n\nb\r"), "line mapper construction")?;
    ensure_eq(&mapper.line_count(), &4, "all physical lines including the final line")?;
    let next_line = ensure_ok(mapper.position(TextSize::from(3)), "the offset after CRLF must map")?;
    ensure(next_line == Position::new(1, 0), "the offset after CRLF must start the next line")?;
    let crlf_interior = ensure_some(
      mapper.position(TextSize::from(2)).err(),
      "the interior of CRLF must not map to an LSP position",
    )?;
    ensure(
      crlf_interior
        == MappingError::OffsetNotBoundary {
          offset: 2
        },
      "the CRLF-interior failure must retain its exact byte offset",
    )?;
    ensure(
      mapper.all_range() == Range::new(Position::new(0, 0), Position::new(3, 0)),
      "a trailing terminator must produce a final empty line",
    )
  }

  /// Assert exact round trips and both invalid endpoint polarities.
  #[test]
  fn round_trips_ranges_and_rejects_invalid_endpoints() -> Result<(), TestFailure> {
    let mapper = ensure_ok(Mapper::new_utf16("alpha\n\u{3b2}eta"), "range mapper construction")?;
    let text_range = TextRange::new(TextSize::from(0), TextSize::from(5));
    let lsp_range = Range::new(Position::new(0, 0), Position::new(0, 5));
    let mapped_range = ensure_ok(mapper.range(text_range), "a valid source range must map")?;
    ensure(mapped_range == lsp_range, "a valid source range must map exactly")?;
    let mapped_text_range = ensure_ok(mapper.text_range(lsp_range), "a valid LSP range must map")?;
    ensure(mapped_text_range == text_range, "a valid LSP range must round-trip exactly")?;
    let offset_out_of_bounds = ensure_some(
      mapper.position(TextSize::from(100)).err(),
      "an out-of-bounds source offset must be rejected",
    )?;
    ensure(
      offset_out_of_bounds
        == MappingError::OffsetOutOfBounds {
          offset:        100,
          source_length: 11,
        },
      "an out-of-bounds source offset must retain the requested and source extents",
    )?;
    let line_out_of_bounds = ensure_some(
      mapper.offset(Position::new(2, 0)).err(),
      "an out-of-bounds LSP line must be rejected",
    )?;
    ensure(
      line_out_of_bounds
        == MappingError::PositionLineOutOfBounds {
          line: 2
        },
      "an out-of-bounds LSP line must retain the requested line",
    )?;
    let column_out_of_bounds = ensure_some(
      mapper.offset(Position::new(0, 6)).err(),
      "an out-of-bounds LSP column must be rejected",
    )?;
    ensure(
      column_out_of_bounds
        == MappingError::PositionCharacterOutOfBounds {
          line:      0,
          character: 6,
        },
      "an out-of-bounds LSP column must retain the requested position",
    )?;
    let reversed = ensure_some(
      mapper.text_range(Range::new(Position::new(1, 0), Position::new(0, 0))).err(),
      "a reversed LSP range must be rejected",
    )?;
    ensure(
      reversed
        == MappingError::ReversedRange {
          start_line:      1,
          start_character: 0,
          end_line:        0,
          end_character:   0,
        },
      "a reversed LSP range must retain both exact endpoints",
    )?;
    let reversed_columns = ensure_some(
      mapper.text_range(Range::new(Position::new(0, 5), Position::new(0, 4))).err(),
      "a same-line range with reversed columns must be rejected",
    )?;
    ensure(
      reversed_columns
        == MappingError::ReversedRange {
          start_line:      0,
          start_character: 5,
          end_line:        0,
          end_character:   4,
        },
      "same-line reversal must retain both exact character columns",
    )?;
    ensure(
      mapper.encoding() == super::PositionEncoding::Utf16,
      "the mapper must report the position encoding selected at construction",
    )
  }

  /// Assert relative positions and ranges use exact start-referenced deltas.
  #[test]
  fn computes_checked_relative_coordinates() -> Result<(), TestFailure> {
    let position = ensure_ok(
      relative_position(Position::new(3, 4), Position::new(1, 8)),
      "a later position must relativize",
    )?;
    ensure(
      position == Position::new(2, 4),
      "a later line must retain its absolute character column",
    )?;
    let range = ensure_ok(
      relative_range(
        Range::new(Position::new(3, 4), Position::new(3, 9)),
        Range::new(Position::new(1, 8), Position::new(1, 10)),
      ),
      "a later range must relativize",
    )?;
    ensure(
      range == Range::new(Position::new(2, 4), Position::new(2, 9)),
      "a single-line range must preserve its character width after relativization",
    )?;
    let same_line = ensure_ok(
      relative_range(
        Range::new(Position::new(1, 10), Position::new(1, 14)),
        Range::new(Position::new(1, 8), Position::new(1, 9)),
      ),
      "a same-line range after its reference must relativize",
    )?;
    ensure(
      same_line == Range::new(Position::new(0, 2), Position::new(0, 6)),
      "same-line endpoints must be measured independently from the reference start",
    )?;
    let underflow = ensure_some(
      relative_position(Position::new(0, 0), Position::new(1, 0)).err(),
      "a position before its reference must be rejected",
    )?;
    ensure(
      underflow
        == MappingError::RelativePositionUnderflow {
          line:                0,
          character:           0,
          reference_line:      1,
          reference_character: 0,
        },
      "relative underflow must retain the absolute and reference positions",
    )?;
    let reversed_reference = ensure_some(
      relative_range(
        Range::new(Position::new(3, 0), Position::new(3, 1)),
        Range::new(Position::new(2, 1), Position::new(2, 0)),
      )
      .err(),
      "a reversed reference range must be rejected before relativization",
    )?;
    ensure(
      reversed_reference
        == MappingError::ReversedRange {
          start_line:      2,
          start_character: 1,
          end_line:        2,
          end_character:   0,
        },
      "a reversed reference must retain its exact endpoints",
    )?;
    let range_underflow = ensure_some(
      relative_range(
        Range::new(Position::new(0, 5), Position::new(0, 6)),
        Range::new(Position::new(0, 6), Position::new(0, 7)),
      )
      .err(),
      "a range beginning before its reference must be rejected",
    )?;
    ensure(
      range_underflow
        == MappingError::RelativePositionUnderflow {
          line:                0,
          character:           5,
          reference_line:      0,
          reference_character: 6,
        },
      "range underflow must retain the exact start and reference positions",
    )
  }

  /// Require every generated Unicode boundary to round-trip in each supported encoding.
  #[test]
  fn generated_offsets_and_positions_round_trip() -> Result<(), TestFailure> {
    let input = (any::<String>(), any::<usize>());
    ensure_property(
      &input,
      "generated Unicode boundaries round-trip through every advertised position encoding",
      |(source, selection)| {
        let boundaries = source
          .char_indices()
          .map(|(offset, _character)| offset)
          .chain(once(source.len()))
          .filter(|offset| {
            (
              source.get(..*offset).is_some_and(|prefix| prefix.ends_with('\r')),
              source.get(*offset..).is_some_and(|suffix| suffix.starts_with('\n')),
            ) != (true, true)
          })
          .collect::<Vec<_>>();
        let selected = ensure_some(
          selection.checked_rem(boundaries.len()),
          "every source must expose at least its final character boundary",
        )?;
        let boundary = ensure_some(
          boundaries.get(selected).copied(),
          "the reduced boundary index must identify a source offset",
        )?;
        let raw = ensure_ok(u32::try_from(boundary), "generated sources must fit Rowan's coordinate width")?;
        let offset = TextSize::from(raw);

        for mapper in [
          ensure_ok(Mapper::new_utf8(&source), "the UTF-8 mapper must construct")?,
          ensure_ok(Mapper::new_utf16(&source), "the UTF-16 mapper must construct")?,
          ensure_ok(Mapper::new_utf32(&source), "the UTF-32 mapper must construct")?,
        ] {
          let position = ensure_ok(
            mapper.position(offset),
            "a generated character boundary must map to a protocol position",
          )?;
          let round_trip = ensure_ok(
            mapper.offset(position),
            "a generated protocol position must map to a source boundary",
          )?;
          ensure(
            round_trip == offset,
            "a generated protocol position must map back to its exact source boundary",
          )?;
        }
        Ok(())
      },
    )
  }
}
