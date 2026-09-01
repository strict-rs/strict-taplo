//! Taplo-specific semantic-token classification and relative wire encoding.

use std::num::TryFromIntError;

use lsp_types::Position;
use lsp_types::SemanticToken;
use lsp_types::SemanticTokenType;
use lsp_types::SemanticTokens;
use lsp_types::SemanticTokensParams;
use lsp_types::SemanticTokensResult;
use taplo::syntax::SyntaxNode;
use taplo::syntax::SyntaxToken;
use taplo::syntax::kind::ARRAY;
use taplo::syntax::kind::IDENT;
use taplo::syntax::kind::INLINE_TABLE;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;
use taplo_lsp_async::util::Mapper;
use taplo_lsp_async::util::MappingError;
use taplo_lsp_async::util::relative_position;
use thiserror::Error as ThisError;

use crate::world::WorldState;

/// A failed semantic-token coordinate or legend projection.
#[derive(Debug, ThisError, Eq, PartialEq)]
enum SemanticTokenError {
  /// A source range cannot be represented in LSP coordinates.
  #[error(transparent)]
  Mapping(#[from] MappingError),
  /// An identifier unexpectedly spans multiple lines.
  #[error("semantic token spans lines {start_line} through {end_line}")]
  Multiline {
    /// Token start line.
    start_line: u32,
    /// Token end line.
    end_line:   u32,
  },
  /// A token type is absent from the advertised legend.
  #[error("semantic token type `{name}` is absent from the advertised legend")]
  MissingTokenType {
    /// Missing token type name.
    name: &'static str,
  },
  /// A legend index cannot fit the wire type.
  #[error("semantic token legend index is not representable by `u32`")]
  LegendIndexOverflow {
    /// Failed integer conversion.
    #[source]
    source: TryFromIntError,
  },
  /// A same-line semantic token has reversed columns.
  #[error("semantic token end column {end} precedes start column {start}")]
  ReversedToken {
    /// Token start column.
    start: u32,
    /// Token end column.
    end:   u32,
  },
}

/// Build Taplo-specific semantic tokens for the requested document snapshot.
///
/// # Errors
///
/// Returns an RPC error when request parameters are absent, source coordinates cannot be
/// represented by LSP, or the document changes while tokens are being built.
macro_rules! define_semantic_token_future_family {
  (
    $semantic_tokens:ident,
    $document_snapshot_for_uri:ident,
    $current_snapshot_response:ident,
    $ensure_current_snapshot:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Build Taplo-specific semantic tokens for the requested document snapshot.
    ///
    /// # Errors
    ///
    /// Returns an RPC error when request parameters are absent, source coordinates cannot be
    /// represented by LSP, or the document changes while tokens are being built.
    #[allow(clippy::single_call_fn, reason = "one semantic-token entry point per execution family, registered exactly once by its runtime family")]
    pub(super) fn $semantic_tokens<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<SemanticTokensParams>,
    ) -> $future<'_, Result<Option<SemanticTokensResult>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;

        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document.uri) => (document_uri, snapshot)
        );
        if !snapshot.config.syntax.semantic_tokens {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        }
        let document = &snapshot.document;
        let Some(syntax) = document.dom.syntax().and_then(|syntax| syntax.as_node()) else {
          super::$ensure_current_snapshot(world, &document_uri, &snapshot).await?;
          return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data:      Vec::new(),
          })));
        };

        let result = SemanticTokensResult::Tokens(SemanticTokens {
          result_id: None,
          data:      create_tokens(syntax, &document.mapper).map_err(|error| RpcError::internal_error().with_details(error.to_string()))?,
        });
        super::$current_snapshot_response(world, &document_uri, &snapshot, Some(result)).await
      })
    }
  };
}

define_document_handler_execution_families!(
  define_semantic_token_future_family;
  current_snapshot_response_local,
  current_snapshot_response_concurrent;
  (semantic_tokens_local, semantic_tokens_concurrent)
  ;
  (
    ensure_current_snapshot_local,
    ensure_current_snapshot_concurrent
  ),
);

/// Taplo-specific semantic token types advertised through the LSP legend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TokenType {
  /// A key whose value is an array.
  TomlArrayKey,
  /// A key whose value is an inline table.
  TomlTableKey,
}

impl TokenType {
  /// Token types advertised to clients in stable index order.
  pub(super) const LEGEND: &'static [SemanticTokenType] = &[SemanticTokenType::new("tomlArrayKey"), SemanticTokenType::new("tomlTableKey")];

  /// Return this type's advertised name.
  const fn name(self) -> &'static str {
    match self {
      Self::TomlArrayKey => "tomlArrayKey",
      Self::TomlTableKey => "tomlTableKey",
    }
  }

  /// Resolve this type through the advertised legend.
  fn legend_index(self) -> Result<u32, SemanticTokenError> {
    let name = self.name();
    let index = Self::LEGEND
      .iter()
      .position(|token_type| token_type.as_str() == name)
      .ok_or(SemanticTokenError::MissingTokenType {
        name,
      })?;
    u32::try_from(index).map_err(|source| SemanticTokenError::LegendIndexOverflow {
      source,
    })
  }
}

/// Classify syntax tokens and encode them in LSP-relative order.
///
/// # Errors
///
/// Returns an error when a source coordinate, token width, or legend index cannot be represented by
/// the LSP semantic-token wire format.
#[tracing::instrument(skip_all)]
fn create_tokens(syntax: &SyntaxNode, mapper: &Mapper) -> Result<Vec<SemanticToken>, SemanticTokenError> {
  let mut builder = SemanticTokensBuilder::new(mapper);

  for element in syntax.descendants_with_tokens() {
    let Some(token) = element.as_token() else {
      continue;
    };
    if token.kind() != IDENT {
      continue;
    }

    // Look for an inline-table value.
    let is_table_key = token
      .parent()
      .and_then(|parent| parent.next_sibling())
      .and_then(|sibling| sibling.first_child())
      .is_some_and(|child| child.kind() == INLINE_TABLE);

    if is_table_key {
      builder.add_token(token, TokenType::TomlTableKey)?;
      continue;
    }

    // Look for an array value.
    let is_array_key = token
      .parent()
      .and_then(|parent| parent.next_sibling())
      .and_then(|sibling| sibling.first_child())
      .is_some_and(|child| child.kind() == ARRAY);

    if is_array_key {
      builder.add_token(token, TokenType::TomlArrayKey)?;
    }
  }

  Ok(builder.build())
}

/// Accumulates ordered semantic tokens while tracking their relative-coordinate origin.
struct SemanticTokensBuilder<'b> {
  /// Encoded tokens in source order.
  tokens:     Vec<SemanticToken>,
  /// Maps Taplo source offsets into LSP positions.
  mapper:     &'b Mapper,
  /// Start position of the previously emitted token.
  last_start: Option<Position>,
}

impl<'b> SemanticTokensBuilder<'b> {
  /// Create an empty builder using `mapper` for source-coordinate projection.
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor keeps the relative-encoding origin private to the builder, so no caller can start a token stream from \
              an already-advanced `last_start` reference position"
  )]
  const fn new(mapper: &'b Mapper) -> Self {
    Self {
      tokens: Vec::new(),
      mapper,
      last_start: None,
    }
  }

  /// Append one classified syntax token.
  ///
  /// # Errors
  ///
  /// Returns an error when the token range cannot be represented as a single-line, forward LSP
  /// semantic token or when its token type is absent from the legend.
  fn add_token(&mut self, token: &SyntaxToken, token_type: TokenType) -> Result<(), SemanticTokenError> {
    let range = self.mapper.range(token.text_range())?;
    if range.start.line != range.end.line {
      return Err(SemanticTokenError::Multiline {
        start_line: range.start.line,
        end_line:   range.end.line,
      });
    }
    let reference = self.last_start.unwrap_or_else(|| Position::new(0, 0));
    let relative = relative_position(range.start, reference)?;
    let length = range
      .end
      .character
      .checked_sub(range.start.character)
      .ok_or(SemanticTokenError::ReversedToken {
        start: range.start.character,
        end:   range.end.character,
      })?;
    self.tokens.push(SemanticToken {
      delta_line: relative.line,
      delta_start: relative.character,
      length,
      token_type: token_type.legend_index()?,
      token_modifiers_bitset: 0,
    });
    self.last_start = Some(range.start);
    Ok(())
  }

  /// Finish relative encoding and return tokens in source order.
  fn build(self) -> Vec<SemanticToken> {
    self.tokens
  }
}

#[cfg(test)]
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use taplo::parser;

  use super::Mapper;
  use super::SemanticToken;
  use super::create_tokens;

  /// Parse one semantic-token fixture and project its custom tokens.
  fn fixture_tokens(source: &str) -> Result<Vec<SemanticToken>, TestFailure> {
    let syntax = ensure_ok(parser::parse(source), "the semantic-token fixture must parse")?.into_syntax();
    let mapper = ensure_ok(Mapper::new_utf16(source), "the semantic-token mapper must build")?;
    ensure_ok(create_tokens(&syntax, &mapper), "semantic-token construction must succeed")
  }

  #[test]
  fn semantic_tokens_follow_the_advertised_legend_and_checked_deltas() -> Result<(), TestFailure> {
    let tokens = fixture_tokens("array = [1]\ntable = { value = 2 }\n")?;
    ensure(
      tokens
        == vec![
          SemanticToken {
            delta_line:             0,
            delta_start:            0,
            length:                 5,
            token_type:             0,
            token_modifiers_bitset: 0,
          },
          SemanticToken {
            delta_line:             1,
            delta_start:            0,
            length:                 5,
            token_type:             1,
            token_modifiers_bitset: 0,
          },
        ],
      "token types, widths, and relative starts must derive from the advertised wire legend",
    )
  }

  #[test]
  fn ordinary_scalar_keys_do_not_receive_custom_semantic_tokens() -> Result<(), TestFailure> {
    let tokens = fixture_tokens("plain = 1\n")?;
    ensure(
      tokens.is_empty(),
      "ordinary scalar keys must not be classified as custom array or table semantic tokens",
    )
  }
}
