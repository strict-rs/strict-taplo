//! Behavioral fixtures for source-preserving TOML formatting.

use std::fmt::Debug;
use std::iter::once;

use strict_test_support::PredicateFailure;
use strict_test_support::ensure_that;

#[cfg(feature = "serde")]
use crate::dom::Node;
use crate::formatter;
use crate::formatter::FormatError;
use crate::formatter::OptionParseError;
use crate::formatter::Options;
use crate::formatter::OptionsIncomplete;
use crate::formatter::ScopedOptions;
use crate::parser::parse;

/// Expected text and the complete native formatting result.
type FormattedObservation = (String, Result<String, FormatError>);

/// Compare a formatter result while retaining its exact behavioral expectation.
fn ensure_formatted(
  expected: &str,
  actual: Result<String, FormatError>,
) -> Result<FormattedObservation, PredicateFailure<FormattedObservation>> {
  ensure_that(
    (expected.to_owned(), actual),
    "formatted TOML must match the exact expected source",
    |observed| observed.1.as_ref().is_ok_and(|formatted| formatted == &observed.0),
  )
}

/// Format one source fixture and retain its complete comparison outcome.
fn ensure_source_format(
  source: &str,
  expected: &str,
  options: &Options,
) -> Result<FormattedObservation, PredicateFailure<FormattedObservation>> {
  ensure_formatted(expected, formatter::format(source, options))
}

/// One exact formatter behavior case with a policy-specific failure context.
#[derive(Debug)]
struct FormatCase<'source> {
  /// Unformatted source.
  source:   &'source str,
  /// Exact formatted source.
  expected: &'source str,
  /// Formatting policy under test.
  options:  Options,
  /// Behavior named by this case.
  context:  &'static str,
}

/// Source, expectation, policy, context, and complete native result for a formatter case.
type FormatObservation = (String, String, Options, &'static str, Result<String, FormatError>);

/// Exercise exact formatter cases without discarding earlier cases or policy context.
fn ensure_format_cases<const CASES: usize>(
  cases: [FormatCase<'_>; CASES],
) -> Result<[FormatObservation; CASES], PredicateFailure<[FormatObservation; CASES]>> {
  let observed = cases.map(|case| {
    let actual = formatter::format(case.source, &case.options);
    (case.source.to_owned(), case.expected.to_owned(), case.options, case.context, actual)
  });
  ensure_that(
    observed,
    "every formatter policy case must retain its exact expected source",
    |values| {
      values
        .iter()
        .all(|value| value.4.as_ref().is_ok_and(|actual| actual == &value.1))
    },
  )
}

/// Build input and expected documents from one line-level formatting map.
fn paired_line_documents(lines: &[(&str, &str)]) -> (String, String) {
  let source = lines.iter().map(|line| line.0).collect::<Vec<_>>().join("\n");
  let expected = lines.iter().map(|line| line.1).collect::<Vec<_>>().join("\n");
  (source, expected)
}

/// Build one consistently indented four-level nested-array document.
fn nested_array_document(values: &[&str]) -> String {
  let elements = values
    .iter()
    .map(|value| format!("    [\n        [\n            [\n                {value},\n            ],\n        ],\n    ],\n"))
    .collect::<Vec<_>>()
    .concat();
  format!("\nmy_array = [\n{elements}]\n")
}

/// Build one four-level nested array with a configurable value suffix and blank-line run.
fn nested_single_value_array(value_suffix: &str, blank_lines: usize) -> String {
  let line_breaks = "\n".repeat(blank_lines.saturating_add(1));
  format!(
    "\nmy_array = [\n    [\n        [\n            [\n                \"my_value\"{value_suffix}{line_breaks}            ]\n        ]\n    ]\n]\n"
  )
}

/// Formatting policy shared by depth-preserving nested-array fixtures.
fn nested_array_policy() -> Options {
  Options {
    array_auto_collapse: false,
    array_trailing_comma: false,
    indent_string: "    ".into(),
    ..Default::default()
  }
}

/// Shared source for enabled/disabled single-comment alignment polarity.
fn single_comment_alignment_source() -> &'static str {
  r#"
entry1 = "string"  # trailing comment
entry2 = "longer_string"

my_array = [
  #Items
  "abc",
  "b", # Some comment
  "caa",
 # comment
  # Other stuff
]
"#
}

/// Keep every syntax-backed public formatter entry point aligned.
#[test]
fn public_entry_points_preserve_syntax_contracts() -> Result<(), impl Debug> {
  let options = Options {
    reorder_keys: true,
    trailing_newline: false,
    ..Options::default()
  };
  let roots = parse("b=2\na=1\n").map(|parsed| {
    let green = formatter::format_green(parsed.green().clone(), &options);
    let syntax = parsed.clone().into_syntax();
    let formatted = formatter::format_syntax(&syntax, &options);
    let entry = syntax.children().next().map(|child| {
      let rendered = formatter::format_syntax(&child, &options);
      (child, rendered)
    });
    (parsed, green, formatted, entry)
  });
  let scoped = parse("value    =1\n").map(|parsed| {
    let full_range = parsed.clone().into_syntax().text_range();
    let formatted = formatter::format_with_scopes(
      &parsed.clone().into_dom(),
      &Options::default(),
      &[full_range],
      ScopedOptions::default(),
    );
    (parsed, formatted)
  });
  ensure_that(
    (roots, scoped),
    "public formatter entry points must preserve root policy, non-root source, and error ranges",
    |observed| {
      observed.0.as_ref().is_ok_and(|root| {
        root.1 == "a = 1\nb = 2" && root.2 == root.1 && root.3.as_ref().is_some_and(|entry| entry.1 == entry.0.to_string())
      }) && observed
        .1
        .as_ref()
        .is_ok_and(|scope| scope.1.as_ref().is_ok_and(|formatted| formatted == "value    =1\n"))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

/// Format detached semantic values through the public scoped formatter.
#[cfg(feature = "serde")]
#[test]
fn detached_public_entry_point_uses_the_typed_renderer() -> Result<(), impl Debug> {
  let options = Options {
    trailing_newline: false,
    ..Options::default()
  };
  let observed = serde_json::from_value::<Node>(serde_json::json!({ "value": 1 })).map(|detached| {
    let formatted = formatter::format_with_scopes(&detached, &options, &[], ScopedOptions::default());
    (detached, formatted)
  });
  ensure_that(
    observed,
    "detached formatting must use the typed renderer and obey trailing-newline policy",
    |result| {
      result
        .as_ref()
        .is_ok_and(|value| value.1.as_ref().is_ok_and(|formatted| formatted == "value = 1"))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

/// Keep compact-array padding and inline-table comments controlled by their declared policies.
#[test]
fn container_formatting_preserves_padding_and_comment_polarities() -> Result<(), impl Debug> {
  let padded = formatter::format("value=[1]\n", &Options {
    array_auto_collapse: false,
    array_auto_expand: false,
    compact_arrays: false,
    ..Options::default()
  });
  let commented = formatter::format("value = {\n  # retained\n  old = 1,\n}\n", &Options::default());
  ensure_that(
    (padded, commented),
    "non-compact arrays must receive symmetric padding and inline-table comments must remain",
    |observed| {
      observed.0.as_ref().is_ok_and(|value| value == "value = [ 1 ]\n")
        && observed.1.as_ref().is_ok_and(|value| value.contains("# retained"))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

/// Parse Boolean, numeric, and textual formatter options through the generated option owner.
#[test]
fn textual_options_parse_each_declared_value_type() -> Result<(), impl Debug> {
  let mut options = Options::default();
  let update = options.update_from_str([("align_entries", "true"), ("column_width", "120"), ("indent_string", "\t")].into_iter());
  ensure_that(
    (options, update),
    "Boolean, numeric, and string options must update their declared fields",
    |observed| observed.1.is_ok() && observed.0.align_entries && observed.0.column_width == 120 && observed.0.indent_string == "\t",
  )
  .map(drop)
  .map_err(Box::new)
}

/// Distinguish a typed option-value failure from an unknown formatter option name.
#[test]
fn textual_options_reject_invalid_values_and_unknown_names() -> Result<(), impl Debug> {
  let mut options = Options::default();
  let invalid = options.update_from_str(once(("align_entries", "not-a-bool")));
  let unknown = options.update_from_str(once(("not_an_option", "true")));
  ensure_that(
    (options, invalid, unknown),
    "option failures must retain invalid values, scalar reasons, and unknown names",
    |observed| {
      matches!(observed.1, Err(OptionParseError::InvalidValue { ref key, ref input, expected: "bool", ref reason })
      if key == "align_entries" && input == "not-a-bool" && !reason.is_empty())
        && observed.2 == Err(OptionParseError::InvalidOption(String::from("not_an_option")))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

/// Expand only arrays whose retained value syntax exceeds the configured column width.
#[test]
fn retained_value_syntax_drives_only_overwidth_array_expansion() -> Result<(), impl Debug> {
  let options = Options {
    array_auto_collapse: false,
    array_auto_expand: true,
    column_width: 24,
    ..Options::default()
  };
  ensure_format_cases([
    FormatCase {
      source:   "long_key = [1, 2, 3, 4, 5]\n",
      expected: "long_key = [\n  1,\n  2,\n  3,\n  4,\n  5,\n]\n",
      options:  options.clone(),
      context:  "over-width arrays must expand",
    },
    FormatCase {
      source: "key = [1, 2]\n",
      expected: "key = [1, 2]\n",
      options,
      context: "short arrays must remain compact",
    },
  ])
  .map(drop)
  .map_err(Box::new)
}

/// Preserve a malformed value-less entry without consuming the following valid entry.
#[test]
fn malformed_entry_without_value_is_preserved_tolerantly() -> Result<(), impl Debug> {
  ensure_that(
    formatter::format("missing =\nnext = 1\n", &Options {
      column_width: 1,
      ..Options::default()
    }),
    "tolerant formatting must preserve missing-value source and its following valid entry",
    |observed| {
      observed
        .as_ref()
        .is_ok_and(|formatted| formatted.contains("missing =") && formatted.contains("next = 1"))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

/// Normalize standalone comment indentation as table nesting changes.
#[test]
fn comment_indentation() -> Result<(), impl Debug> {
  let (source, mut expected) = paired_line_documents(&[
    ("# aaasd", "# aaasd"),
    ("", ""),
    ("[profile]", "[profile]"),
    ("", ""),
    ("# asd", "# asd"),
    ("   # asd", "# asd"),
    ("", ""),
    ("# bsd ", "# bsd "),
    (" # bsd", "# bsd"),
    ("asd = \"\"", "asd = \"\""),
    ("", ""),
    ("# csd", "  # csd"),
    ("    [profile.release]", "  [profile.release]"),
    ("", ""),
    ("    incremental  = true ", "  incremental = true"),
    ("    lol = 2 #yo", "  lol = 2            #yo"),
    (
      "    debug = 0          # Set this to 1 or 2 to get more useful backtraces in debugger.",
      "  debug = 0          # Set this to 1 or 2 to get more useful backtraces in debugger.",
    ),
    ("", ""),
    ("    # asd", "  # asd"),
  ]);
  expected.push('\n');
  ensure_source_format(&source, &expected, &Options {
    indent_tables: true,
    ..Default::default()
  })
  .map(drop)
  .map_err(Box::new)
}

/// Preserve the blank-line boundary following an entry with an inline comment.
#[test]
fn comment_after_entry() -> Result<(), impl Debug> {
  let expected = "incremental = true

debug = 0 # Set this to 1 or 2 to get more useful backtraces in debugger.
";

  let formatted = formatter::format(expected, &Options::default());

  ensure_formatted(expected, formatted).map(drop)
}

/// Keep comments attached to the table header or entry that follows them.
#[test]
fn comment_before_entry() -> Result<(), impl Debug> {
  let expected = "

# hello
[lib]
# bello
incremental = true
";

  let formatted = formatter::format(expected, &Options::default());

  ensure_formatted(expected, formatted).map(drop)
}

/// Align scalar and composite entries against one shared trailing-comment column.
#[test]
fn align_composite_entries() -> Result<(), impl Debug> {
  let src = r#"k1 = 1                                                      # 111
k2 = false                                                  # 222
k3 = "public"                                               # 333
k4 = ["/home/www", "/var/lib/www"] # 4444444444444444444444
k6 = {a="yes", table="yes"} # 4444444444444444444444
k5 = false                                                  # 555
"#;

  let formatted = formatter::format(src, &Options {
    align_entries: true,
    ..Default::default()
  });

  let expected = r#"k1 = 1                             # 111
k2 = false                         # 222
k3 = "public"                      # 333
k4 = ["/home/www", "/var/lib/www"] # 4444444444444444444444
k6 = { a = "yes", table = "yes" }  # 4444444444444444444444
k5 = false                         # 555
"#;

  ensure_formatted(expected, formatted).map(drop)
}

/// Remove whitespace-only lines and cap excessive blank separation between tables.
#[test]
fn test_space_in_line() -> Result<(), impl Debug> {
  let src = r#" 
[foo]
 
foo = "bar"
 
bar = "foo"
 

 

 

[bar]
foo = "bar"
"#;
  let formatted = formatter::format(src, &Options {
    align_entries: true,
    ..Default::default()
  });

  let expected = r#"
[foo]

foo = "bar"

bar = "foo"


[bar]
foo = "bar"
"#;

  ensure_formatted(expected, formatted).map(drop)
}

/// Preserve an explanatory array-item comment and the array's closing comment.
#[test]
fn test_comment_in_array() -> Result<(), impl Debug> {
  let expected = r#"
[features]
myfeature = [
  "feature1",
  # needed because blah blah blah reason that only makes sense when attached to feature2
  "feature2",
] # comment2
nextfeature = []
"#;
  let formatted = formatter::format(expected, &Options {
    align_entries: false,
    ..Default::default()
  });

  ensure_formatted(expected, formatted).map(drop)
}

/// Preserve grouped standalone, disabled-item, and inline comments within an array.
#[test]
fn test_comments_in_array() -> Result<(), impl Debug> {
  let expected = r#"
[main]
my_array = [
  #Items
  "a",
  "b", # Some comment
  "c", # This is special

  # Other items
  "d",
  "e",
  "f",

  # Some other items we decided not to include
  # "g",
  # "h",
  # "i",

  "item",
]
"#;

  let formatted = formatter::format(expected, &Options::default());

  ensure_formatted(expected, formatted).map(drop)
}

/// Align trailing comments separately within root entries and array elements.
#[test]
fn test_align_comments() -> Result<(), impl Debug> {
  let src = r#"
entry1 = "string"  # trailing comment
entry2 = "longer_string"  # trailing comment

my_array = [
  #Items
  "abc",  # comment
  "b", # Some comment
  "caa",    # This is special
 # comment
  # Other stuff
]
"#;

  let expected = r#"
entry1 = "string"        # trailing comment
entry2 = "longer_string" # trailing comment

my_array = [
  #Items
  "abc", # comment
  "b",   # Some comment
  "caa", # This is special
  # comment
  # Other stuff
]
"#;

  let formatted = formatter::format(src, &Options {
    align_comments: true,
    ..Default::default()
  });

  ensure_formatted(expected, formatted).map(drop)
}

/// Exercise entry and trailing-comment alignment as independent policies.
#[test]
fn entry_and_comment_alignment_policies_are_independent() -> Result<(), impl Debug> {
  ensure_format_cases([
    FormatCase {
      source:   "\nentry1asdasd = \"string\"     # trailing comment\nentry2asd = \"longer_string\" # trailing comment\na = \
                 \"longer_string_hm\"      # trailing comment\n",
      expected: "\nentry1asdasd = \"string\"     # trailing comment\nentry2asd = \"longer_string\" # trailing comment\na = \
                 \"longer_string_hm\"      # trailing comment\n",
      options:  Options {
        align_comments: true,
        align_entries: false,
        ..Default::default()
      },
      context:  "comment alignment must remain active when entry alignment is disabled",
    },
    FormatCase {
      source:
        "\nentry1asdasd =  \"string\"     # trailing comment\nentry2asd   = \"longer_string\"        # trailing comment\na         = \
         \"longer_string_hm\" # trailing comment\n",
      expected: "\nentry1asdasd = \"string\" # trailing comment\nentry2asd    = \"longer_string\" # trailing comment\na            = \
                 \"longer_string_hm\" # trailing comment\n",
      options:  Options {
        align_comments: false,
        align_entries: true,
        ..Default::default()
      },
      context:  "entry alignment must remain active when comment alignment is disabled",
    },
  ])
  .map(drop)
  .map_err(Box::new)
}

/// Insert the required trailing comma while preserving nested-array indentation.
#[test]
fn test_nested_arrays() -> Result<(), impl Debug> {
  let src = r#"
my_array = [
    [
        "my_value",
    ]
]
"#;

  let expected = r#"
my_array = [
    [
        "my_value",
    ],
]
"#;

  ensure_source_format(src, expected, &Options {
    align_comments: false,
    align_entries: true,
    array_auto_collapse: false,
    indent_string: "    ".into(),
    ..Default::default()
  })
  .map(drop)
  .map_err(Box::new)
}

/// Expand an array only after its formatted entry crosses the configured width boundary.
#[test]
fn test_too_long_array() -> Result<(), impl Debug> {
  let src = r#"
array_is_just_right = ["this_line_is_exactly_80_characters_long", "filler_data"]
"#;

  let expected = r#"
array_is_just_right = ["this_line_is_exactly_80_characters_long", "filler_data"]
"#;

  let formatted = formatter::format(src, &Options {
    array_auto_collapse: false,
    array_auto_expand: true,
    indent_string: "    ".into(),
    ..Default::default()
  });

  let ordinary = ensure_formatted(expected, formatted);

  let narrow_source = r#"
array_is_a_bit_too_long = ["this_line_is_exactly_80_characters_long", "filler_data"]
"#;

  let narrow_expected = r#"
array_is_a_bit_too_long = [
    "this_line_is_exactly_80_characters_long",
    "filler_data",
]
"#;

  let narrow_formatted = formatter::format(narrow_source, &Options {
    array_auto_collapse: false,
    array_auto_expand: true,
    column_width: 80,
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_that(
    (ordinary, ensure_formatted(narrow_expected, narrow_formatted)),
    "width boundaries must preserve both ordinary and expanded array formatting",
    |observed| observed.0.is_ok() && observed.1.is_ok(),
  )
  .map(drop)
  .map_err(Box::new)
}

/// Keep a representative Cargo manifest byte-stable under its formatter configuration.
#[test]
fn test_cargo_toml() -> Result<(), impl Debug> {
  let src = r#"
[package]
authors = ["tamasfe"]
categories = ["parser-implementations", "parsing"]
description = "A TOML parser, analyzer and formatter library"
edition = "2018"
homepage = "https://taplo.tamasfe.dev"
keywords = ["toml", "parser", "formatter", "linter"]
license = "MIT"
name = "taplo"
readme = "../README.md"
repository = "https://github.com/tamasfe/taplo"
version = "0.5.4"

[lib]
crate-type = ["cdylib", "lib"]

[features]
serde = ["serde_crate", "serde_json"]
schema = ["once_cell", "schemars", "serde"]
rewrite = []

[dependencies]
glob = "0.3"
indexmap = "1.6.2"
logos = "0.12.0"
regex = "1.5.4"
rowan = "0.12.6"
semver = { version = "1.0.3", features = ["serde"] }
smallvec = "1.6.1"

chrono = { version = "0.4", optional = true }
time = { version = "0.2", optional = true }

once_cell = { version = "1.8.0", optional = true }
schemars = { version = "0.8.3", optional = true }
serde_crate = { package = "serde", version = "1", features = ["derive"], optional = true }
serde_json = { version = "1", optional = true }
verify = { version = "0.3", features = ["schemars", "serde"], optional = true }

[target.'cfg(target_arch = "wasm32")'.dependencies]
wasm-bindgen = { version = "0.2", features = ["serde-serialize"] }
toml = "0.7"

[dev-dependencies]
assert-json-diff = "2"
serde_json = "1"
toml = "0.7"
difference = "2.0.0"

[package.metadata.docs.rs]
features = ["serde", "schema", "chrono", "rewrite"]
"#;

  ensure_source_format(src, src, &Options {
    array_auto_collapse: false,
    array_auto_expand: true,
    column_width: 90,
    indent_string: "    ".into(),
    ..Default::default()
  })
  .map(drop)
  .map_err(Box::new)
}

/// Preserve deeply nested arrays, tables, and inline tables without flattening their layout.
#[test]
fn test_very_nested_arrays() -> Result<(), impl Debug> {
  let source = nested_array_document(&["\"my_value\"", "\"my_value\"", "[{ even = { more = [\"nested\"] } }]"]);

  ensure_source_format(&source, &source, &Options {
    array_auto_collapse: false,
    indent_string: "    ".into(),
    ..Default::default()
  })
  .map(drop)
  .map_err(Box::new)
}

/// Collapse a comment-free nested array hierarchy when compact collapsing is enabled.
#[test]
fn array_collapse() -> Result<(), impl Debug> {
  let source = nested_array_document(&["\"my_value\""]);

  let expected = r#"
my_array = [[[["my_value"]]]]
"#;

  ensure_source_format(&source, expected, &Options {
    array_auto_collapse: true,
    compact_arrays: true,
    indent_string: "    ".into(),
    ..Default::default()
  })
  .map(drop)
  .map_err(Box::new)
}

/// Append exactly one terminal newline when trailing newlines are enabled.
#[test]
fn trailing_newline() -> Result<(), impl Debug> {
  let src = "trailing_new_line = {}";

  let expected = "trailing_new_line = {}
";

  let formatted = formatter::format(src, &Options {
    array_auto_collapse: true,
    compact_arrays: true,
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(expected, formatted).map(drop)
}

/// Remove the terminal newline when the formatter explicitly disables it.
#[test]
fn no_trailing_newline() -> Result<(), impl Debug> {
  let src = "no_new_line = {}
";

  let expected = "no_new_line = {}";

  ensure_source_format(src, expected, &Options {
    array_auto_collapse: true,
    compact_arrays: true,
    trailing_newline: false,
    indent_string: "    ".into(),
    ..Default::default()
  })
  .map(drop)
  .map_err(Box::new)
}

/// Compact assignment and inline-table separators while retaining aligned comments.
#[test]
fn test_compact_entries() -> Result<(), impl Debug> {
  let src = r#"
entry1asdasd =  "string"     # trailing comment
entry2asd   = "longer_string"        # trailing comment
a         = "longer_string_hm" # trailing comment
inline_table = { key = "value" }
"#;

  let expected = r#"
entry1asdasd="string"        # trailing comment
entry2asd="longer_string"    # trailing comment
a="longer_string_hm"         # trailing comment
inline_table={ key="value" }
"#;

  let formatted = formatter::format(src, &Options {
    align_comments: true,
    align_entries: false,
    compact_entries: true,
    ..Default::default()
  });

  ensure_formatted(expected, formatted).map(drop)
}

/// Apply line-ending and inline-container spacing policies independently.
#[test]
fn line_endings_and_inline_table_padding_follow_independent_policies() -> Result<(), impl Debug> {
  let line_endings = ensure_source_format("first=1\nsecond=2", "first = 1\r\nsecond = 2\r\n", &Options {
    crlf: true,
    ..Options::default()
  });
  let padding = ensure_source_format(
    "inline = { first = 1, second = 2 }\n",
    "inline = {first = 1, second = 2}\n",
    &Options {
      compact_inline_tables: true,
      ..Options::default()
    },
  );
  ensure_that(
    (line_endings, padding),
    "line endings and inline-table padding must follow independent policies",
    |observed| observed.0.is_ok() && observed.1.is_ok(),
  )
  .map(drop)
  .map_err(Box::new)
}

/// Omit trailing commas at every level of a nested array when that policy is disabled.
#[test]
fn array_no_trailing_comma() -> Result<(), impl Debug> {
  ensure_source_format(
    &nested_single_value_array(",", 0),
    &nested_single_value_array("", 0),
    &nested_array_policy(),
  )
  .map(drop)
}

/// Bound a long run of blank lines inside a nested array to the configured maximum.
#[test]
fn array_max_new_lines() -> Result<(), impl Debug> {
  ensure_source_format(
    &nested_single_value_array("", 11),
    &nested_single_value_array("", 2),
    &nested_array_policy(),
  )
  .map(drop)
}

/// Preserve entry and table indentation through nested tables and repeated table arrays.
#[test]
fn indent_entries() -> Result<(), impl Debug> {
  let src = r#"
[table]

  entry = "stuff"

  [table.subtable]
    nested_entry = 2

    [[table.subtable.array]]
      entry_array = [
        "value",
        [
          "nested_value"
        ]
      ]

    [[table.subtable.array]]
      entry_array = [
        "value",
        [
          "nested_value"
        ]
      ]

[not_sub_table]

  another_entry = 3
"#;

  let formatted = formatter::format(src, &Options {
    array_auto_collapse: false,
    array_trailing_comma: false,
    indent_entries: true,
    indent_tables: true,
    indent_string: "  ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Preserve distinct comment groups around headers, entries, arrays, and end of input.
#[test]
fn multiple_comments() -> Result<(), impl Debug> {
  let src = r#"
# comments at the start
# comments at the start
# comments at the start

[table1] # comment after table

# comment before table
[table2] # comment after table
# comment under table

# multiple
# comment
# lines
entry = "value"

entry_2 = true # comment
# comment
# comment

# free-standing comments
# free-standing comments
# free-standing comments

# table comment
# table comment
[table3]
# comment after table
# comment after table
another_entry = 2

# free-standing comments
# free-standing comments
# free-standing comments

array = [ # comment at start
    "value",
    # multiple comments in array
    # multiple comments in array
    # multiple comments in array
    # multiple comments in array

    # multiple comments in array
    # multiple comments in array

    "value",

    # multiple comments in array
    # multiple comments in array
    "value",
] # trailing comment
# trailing comment under

# trailing comments
# trailing comments
# trailing comments
"#;

  let formatted = formatter::format(src, &Options {
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Indent entry-owned comments with their table while leaving root header comments unindented.
#[test]
fn multiple_comments_indented() -> Result<(), impl Debug> {
  let src = "
#General settings
[general]
    #Is Enabled?
    enabled = true
    #Cost
    #Range: > -2147483648
    cost = 10
    #Is Starter Glyph?
    starter = false
    #The maximum number of times this glyph may appear in a single spell
    #Range: > 1
    per_spell_limit = 2147483647

# table comments
# table comments
# table comments
[another_table]
    # comment under table
    # comment under table
";

  let formatted = formatter::format(src, &Options {
    indent_entries: true,
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Avoid inventing blank lines between adjacent sibling table sections.
#[test]
fn table_entries_no_blank_space() -> Result<(), impl Debug> {
  let src = r#"
[a]
hello = "world"
[b]
foo = ["bar"]
"#;

  let formatted = formatter::format(src, &Options {
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Keep adjacent sibling tables contiguous when their entries are indented.
#[test]
fn table_entries_no_blank_space_indent_entries() -> Result<(), impl Debug> {
  let src = r#"
[a]
    hello = "world"
[b]
    foo = ["bar"]
"#;

  let formatted = formatter::format(src, &Options {
    indent_entries: true,
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Keep nested table headers contiguous under combined table and entry indentation.
#[test]
fn table_entries_no_blank_space_indent_entries_and_tables() -> Result<(), impl Debug> {
  let src = r#"
[a]
    hello = "world"
    [a.b]
        foo = ["bar"]
"#;

  let formatted = formatter::format(src, &Options {
    indent_entries: true,
    indent_tables: true,
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Preserve a comment-only array body without manufacturing an element or comma.
#[test]
fn single_comment_in_array() -> Result<(), impl Debug> {
  let src = "
runtime-benchmarks = [
    # a comment
]
";

  let formatted = formatter::format(src, &Options {
    indent_entries: true,
    indent_tables: true,
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Derive stable indentation for nested tables across repeated array-of-table parents.
#[test]
fn table_indents() -> Result<(), impl Debug> {
  let src = r#"
[[table]]
    name = "Root Table 1"
    [table.nestedtable]
        name = "Nested parent"
        [[table.nestedtable.subtable]]
            name = "Subtable 1"
        [[table.nestedtable.subtable]]
            name = "Subtable 2"

[[table]]
    name = "Root Table 2"
"#;

  let formatted = formatter::format(src, &Options {
    indent_entries: true,
    indent_tables: true,
    indent_string: "    ".into(),
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Keep an over-width inline table on one line when expansion is disabled.
#[test]
fn no_expand_inline_table() -> Result<(), impl Debug> {
  let src = r#"
very_long_inline_table = { array = ["aaaaa", "aaaaa", "aaaaa", "aaaaa", "aaaaa", "aaaaa", "aaaaa", "aaaaa", "aaaaa"] }
"#;

  let formatted = formatter::format(src, &Options {
    indent_string: "  ".into(),
    inline_table_expand: false,
    ..Default::default()
  });

  ensure_formatted(src, formatted).map(drop)
}

/// Reorder inline-table keys recursively without disturbing surrounding document order.
#[test]
fn test_sorted_inline_tables() -> Result<(), impl Debug> {
  let src = "
foo = { b = 2, a = 1 }

bar = [
  { a = 1, b = 2, c = 3 },
  { b = 2, a = 1, d = 4, e = 5 },
]
";

  let expected = "
foo = { a = 1, b = 2 }

bar = [{ a = 1, b = 2, c = 3 }, { a = 1, b = 2, d = 4, e = 5 }]
";

  let formatted = formatter::format(src, &Options {
    reorder_inline_tables: true,
    ..Default::default()
  });

  ensure_formatted(expected, formatted).map(drop)
}

/// Sort values within blank-line-delimited array groups while preserving group boundaries.
#[test]
fn test_sorted_groupings_in_array() -> Result<(), impl Debug> {
  let src = r#"
foo = [
  "b",
  "a",
  "c",

  2021-01-01,
  1979-05-27,

  ["x", "a"],
  { b = 2, a = 1 },

  3,
  1,
  2,
  10, # due to the lexicographic order
  3
]
"#;

  let expected = r#"
foo = [
  "a",
  "b",
  "c",

  1979-05-27,
  2021-01-01,

  ["a", "x"],
  { b = 2, a = 1 },

  1,
  10, # due to the lexicographic order
  2,
  3,
  3,
]
"#;

  let formatted = formatter::format(src, &Options {
    reorder_arrays: true,
    ..Default::default()
  });

  ensure_formatted(expected, formatted).map(drop)
}

/// Exercise isolated trailing-comment alignment in both policy polarities.
#[test]
fn single_comment_alignment_obeys_both_policy_polarities() -> Result<(), impl Debug> {
  let (disabled_expected, enabled_expected) = paired_line_documents(&[
    ("", ""),
    (
      r#"entry1 = "string" # trailing comment"#,
      r#"entry1 = "string"        # trailing comment"#,
    ),
    (r#"entry2 = "longer_string""#, r#"entry2 = "longer_string""#),
    ("", ""),
    ("my_array = [", "my_array = ["),
    ("  #Items", "  #Items"),
    (r#"  "abc","#, r#"  "abc","#),
    (r#"  "b", # Some comment"#, r#"  "b",   # Some comment"#),
    (r#"  "caa","#, r#"  "caa","#),
    ("  # comment", "  # comment"),
    ("  # Other stuff", "  # Other stuff"),
    ("]", "]"),
    ("", ""),
  ]);

  ensure_format_cases([
    FormatCase {
      source:   single_comment_alignment_source(),
      expected: &disabled_expected,
      options:  Options {
        align_comments: true,
        align_single_comments: false,
        ..Default::default()
      },
      context:  "disabled single-comment alignment must leave isolated comments unpadded",
    },
    FormatCase {
      source:   single_comment_alignment_source(),
      expected: &enabled_expected,
      options:  Options {
        align_comments: true,
        align_single_comments: true,
        ..Default::default()
      },
      context:  "enabled single-comment alignment must pad isolated trailing comments",
    },
  ])
  .map(drop)
  .map_err(Box::new)
}

/// Treat brackets inside an array comment as comment text rather than delimiters.
#[test]
fn test_comment_with_brackets() -> Result<(), impl Debug> {
  let src = r#"
my_array = [
  # [x]
  "y",
]
"#;

  let expected = r#"
my_array = [
  # [x]
  "y",
]
"#;

  let formatted = formatter::format(src, &Options::default());

  ensure_formatted(expected, formatted).map(drop)
}

/// Retain an inline entry comment even when the configured column width is one.
#[test]
fn test_comment_after_entry() -> Result<(), impl Debug> {
  let src = r#"
a = "b" # comment
"#;

  let expected = r#"
a = "b" # comment
"#;
  let opt = Options {
    column_width: 1,
    ..Default::default()
  };
  let formatted = formatter::format(src, &opt);

  ensure_formatted(expected, formatted).map(drop)
}

/// Apply an array-reordering rule only to the exact path selected by its scope.
#[test]
fn test_entry_rule() -> Result<(), impl Debug> {
  let src = r#"
[foo]
sort_me = ["3", "2", "1"]
sort_me_not = ["3", "2", "1"]
"#;

  let expected = r#"
[foo]
sort_me = ["1", "2", "3"]
sort_me_not = ["3", "2", "1"]
"#;

  let observed = parse(src).map(|parsed| {
    let scopes = [("foo.sort_me", OptionsIncomplete {
      reorder_arrays: Some(true),
      ..Default::default()
    })];
    let formatted = formatter::format_with_path_scopes(&parsed.clone().into_dom(), &Options::default(), &[], scopes);
    (parsed, ensure_formatted(expected, formatted))
  });
  ensure_that(
    observed,
    "path-scoped array reordering must affect only the selected path",
    |result| result.as_ref().is_ok_and(|value| value.1.is_ok()),
  )
  .map(drop)
  .map_err(Box::new)
}
