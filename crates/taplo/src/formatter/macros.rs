//! Generation of complete and partial formatter option structures.
//!
//! The expansion owns field-by-field updates and type-directed textual parsing so callers cannot
//! create a parallel option vocabulary or erase the concrete parse failure.

/// Generate complete formatter options plus snake-case and camel-case partial update forms.
macro_rules! create_options {
    (
        $(#[$attr:meta])*
        pub struct Options {
            $(
                $(#[$field_attr:meta])*
                pub $name:ident: $ty:ty,
            )+
        }
    ) => {
        #[cfg_attr(feature = "schema", derive(JsonSchema))]
        $(#[$attr])*
        pub struct Options {
            $(
                $(#[$field_attr])*
                pub $name: $ty,
            )+
        }

        impl Options {
            /// Apply every present snake-case option value.
            pub fn update(&mut self, incomplete: OptionsIncomplete) {
                $(
                    if let Some(option_value) = incomplete.$name {
                        self.$name = option_value;
                    }
                )+
            }

            /// Apply every present camel-case option value.
            pub fn update_camel(&mut self, incomplete: OptionsIncompleteCamel) {
                $(
                    if let Some(option_value) = incomplete.$name {
                        self.$name = option_value;
                    }
                )+
            }

            /// Parse and apply textual snake-case option values.
            ///
            /// # Errors
            ///
            /// Returns [`OptionParseError`] for an unknown option or a value
            /// that cannot be parsed as the option's declared type.
            pub fn update_from_str<S: AsRef<str>, I: Iterator<Item = (S, S)>>(
                &mut self,
                values: I,
            ) -> Result<(), OptionParseError> {
                for (key, val) in values {

                    $(
                        if key.as_ref() == stringify!($name) {
                            self.$name =
                                val.as_ref()
                                    .parse::<$ty>()
                                    .map_err(|error| OptionParseError::InvalidValue {
                                        key: key.as_ref().into(),
                                        input: val.as_ref().into(),
                                        expected: stringify!($ty),
                                        reason: error.to_string(),
                                    })?;

                            continue;
                        }
                    )+

                    return Err(OptionParseError::InvalidOption(key.as_ref().into()));
                }

                Ok(())
            }
        }

        #[cfg_attr(feature = "schema", derive(JsonSchema))]
        /// Optional snake-case formatter fields for range-scoped and incremental updates.
        #[derive(Debug, Clone, Eq, PartialEq, Default)]
        #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
        pub struct OptionsIncomplete {
            $(
                $(#[$field_attr])*
                pub $name: Option<$ty>,
            )+
        }

        impl OptionsIncomplete {
            /// Convert complete options into the snake-case partial form.
            pub fn from_options(opts: Options) -> Self {
                let mut incomplete_options = Self::default();

                $(
                    incomplete_options.$name = Some(opts.$name);
                )+

                incomplete_options
            }
        }

        #[cfg_attr(feature = "schema", derive(JsonSchema))]
        /// Optional camel-case formatter fields for JavaScript and configuration projections.
        #[derive(Debug, Clone, Eq, PartialEq, Default)]
        #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
        #[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
        pub struct OptionsIncompleteCamel {
            $(
                $(#[$field_attr])*
                pub $name: Option<$ty>,
            )+
        }

        impl OptionsIncompleteCamel {
            /// Convert complete options into the camel-case partial form.
            pub fn from_options(opts: Options) -> Self {
                let mut incomplete_options = Self::default();

                $(
                    incomplete_options.$name = Some(opts.$name);
                )+

                incomplete_options
            }
        }
    };
}
