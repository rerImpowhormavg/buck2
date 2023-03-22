/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Implements target pattern resolution.
//!
#![doc = include_str!("target_pattern.md")]

mod ascii_pattern;
pub mod parse_package;

use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;

use allocative::Allocative;
use anyhow::Context;
use dupe::Dupe;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::cells::cell_path::CellPath;
use crate::cells::cell_path::CellPathRef;
use crate::cells::paths::CellRelativePath;
use crate::cells::CellAlias;
use crate::cells::CellAliasResolver;
use crate::fs::paths::forward_rel_path::ForwardRelativePath;
use crate::package::PackageLabel;
use crate::pattern::ascii_pattern::split1_opt_ascii;
use crate::pattern::ascii_pattern::strip_suffix_ascii;
use crate::pattern::ascii_pattern::trim_prefix_ascii;
use crate::pattern::ascii_pattern::AsciiChar;
use crate::pattern::ascii_pattern::AsciiStr;
use crate::pattern::ascii_pattern::AsciiStr2;
use crate::provider::flavors::map_flavors;
use crate::provider::label::NonDefaultProvidersName;
use crate::provider::label::ProviderName;
use crate::provider::label::ProvidersLabel;
use crate::provider::label::ProvidersName;
use crate::target::label::TargetLabel;
use crate::target::name::TargetName;
use crate::target::name::TargetNameRef;
use crate::target_aliases::TargetAliasResolver;

#[derive(thiserror::Error, Debug)]
enum TargetPatternParseError {
    #[error("Expected a `:`, a trailing `/...` or the literal `...`.")]
    UnexpectedFormat,
    #[error("Package is empty")]
    PackageIsEmpty,
    #[error("Must be absolute, with a `//` or no package just `:`.")]
    AbsoluteRequired,
    #[error(
        "Packages may not end with a trailing `/` (except when provided on the command line where it's tolerated)"
    )]
    PackageTrailingSlash,
    #[error("Required a target literal, but got a non-literal pattern `{0}`")]
    TargetLiteralRequired(String),
    #[error(
        "You may be trying to use a macro instead of a target pattern. Macro usage is invalid here"
    )]
    PossibleMacroUsage,
    #[error("Expecting target name, without providers")]
    ExpectingTargetNameWithoutProviders,
    #[error("Expecting target pattern, without providers, got: `{0}`")]
    ExpectingTargetPatternWithoutProviders(String),
}

/// The pattern type to be parsed from the command line target patterns.
///
/// This is either 'TargetLabel', 'ConfiguredTargetLabel', or
/// 'ConfiguredProvidersLabel'
pub trait PatternType:
    Sized + Clone + Default + Display + Debug + PartialEq + Eq + Ord + Allocative
{
    /// Construct this from a TargetName and the ExtraParts.
    fn from_parts(providers: ProvidersName) -> anyhow::Result<Self>;
}

/// Pattern that matches an explicit target without any inner providers label.
/// This is useful for 'query's where we do not expect any provider specifiers.
///
/// Ex. `//some/package:target`
#[derive(
    derive_more::Display,
    Clone,
    Default,
    Debug,
    Eq,
    PartialEq,
    Hash,
    Ord,
    PartialOrd,
    Allocative
)]
#[display(fmt = "")]
pub struct TargetPatternExtra;

impl PatternType for TargetPatternExtra {
    fn from_parts(providers: ProvidersName) -> anyhow::Result<Self> {
        if providers != ProvidersName::Default {
            return Err(TargetPatternParseError::ExpectingTargetNameWithoutProviders.into());
        }
        Ok(TargetPatternExtra)
    }
}

/// Pattern that matches an inner providers label that refers to a specific
/// set of providers from a rule.
/// This is useful for builds, and provider or action queries where provider
/// specifiers makes sense
///
/// Ex. `//some/package:target[java-group]`
#[derive(
    derive_more::Display,
    Clone,
    Default,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Allocative
)]
pub struct ProvidersPatternExtra {
    pub providers: ProvidersName,
}

impl ProvidersPatternExtra {
    pub fn into_providers_label(
        self,
        package: PackageLabel,
        target_name: &TargetNameRef,
    ) -> ProvidersLabel {
        ProvidersLabel::new(TargetLabel::new(package, target_name), self.providers)
    }
}

impl PatternType for ProvidersPatternExtra {
    fn from_parts(providers: ProvidersName) -> anyhow::Result<Self> {
        Ok(ProvidersPatternExtra { providers })
    }
}

/// Extract provider name from a target pattern.
pub(crate) fn split_providers_name(s: &str) -> anyhow::Result<(&str, ProvidersName)> {
    if let Some((t, flavors)) = split1_opt_ascii(s, AsciiChar::new('#')) {
        let name = map_flavors(flavors, s)?;
        Ok((t, name))
    } else if let Some((t, p)) = split1_opt_ascii(s, AsciiChar::new('[')) {
        let mut names = Vec::new();

        let mut remaining = if let Some((p, r)) = split1_opt_ascii(p, AsciiChar::new(']')) {
            names.push(ProviderName::new(p.to_owned())?);
            r
        } else {
            return Err(anyhow::anyhow!(
                "target pattern with `[` must end with `]` to mark end of providers set label"
            ));
        };

        while !remaining.is_empty() {
            if let Some(("", r)) = split1_opt_ascii(remaining, AsciiChar::new('[')) {
                if let Some((p, r)) = split1_opt_ascii(r, AsciiChar::new(']')) {
                    names.push(ProviderName::new(p.to_owned())?);
                    remaining = r;
                    continue;
                }
            }
            return Err(anyhow::anyhow!(
                "target pattern with `[` must end with `]` to mark end of providers set label"
            ));
        }

        Ok((
            t,
            ProvidersName::NonDefault(Box::new(NonDefaultProvidersName::Named(
                names.into_boxed_slice(),
            ))),
        ))
    } else {
        Ok((s, ProvidersName::Default))
    }
}

/// A parsed target pattern.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Allocative)]
pub enum ParsedPattern<T: PatternType> {
    /// A target pattern that matches a explicit target pattern type T. See
    /// `PatternType` for pattern
    Target(PackageLabel, TargetName, T),
    /// A target pattern that matches an entire package. Ex. `//some/package:`
    Package(PackageLabel),
    /// A target pattern that matches all recursive packages. Ex.
    /// `//some/package/...`. The path component here is not required to be
    /// an actual package (i.e. a build file is not required at the path)
    /// and so we don't hold this as a [PackageLabel].
    Recursive(CellPath),
}

impl ParsedPattern<TargetPatternExtra> {
    /// Extract [`TargetLabel`] from a [`ParsedPattern`].
    pub fn as_target_label(self, original: &str) -> anyhow::Result<TargetLabel> {
        let (package, target_name, TargetPatternExtra) = self.as_literal(original)?;
        Ok(TargetLabel::new(package, target_name.as_ref()))
    }

    /// Check if a [`ParsedPattern`] matches a [`TargetLabel`]
    pub fn matches(&self, target: &TargetLabel) -> bool {
        let target_pkg = target.pkg();
        match self {
            ParsedPattern::Target(pkg, t, TargetPatternExtra) => {
                *pkg == target_pkg && t.as_ref() == target.name()
            }
            ParsedPattern::Package(pkg) => target_pkg.as_cell_path() == pkg.as_cell_path(),
            ParsedPattern::Recursive(cell_path) => {
                target_pkg.as_cell_path().starts_with(cell_path.as_ref())
            }
        }
    }
}

impl ParsedPattern<ProvidersPatternExtra> {
    /// Extract [`ProvidersLabel`] from a [`ParsedPattern`].
    pub fn as_providers_label(self, original: &str) -> anyhow::Result<ProvidersLabel> {
        let (package, target, ProvidersPatternExtra { providers }) = self.as_literal(original)?;
        Ok(ProvidersLabel::new(
            TargetLabel::new(package, target.as_ref()),
            providers,
        ))
    }
}

impl<T: PatternType> ParsedPattern<T> {
    /// Extract a literal from a [ParsedPattern], or `Err` if it is not a literal.
    pub fn as_literal(self, original: &str) -> anyhow::Result<(PackageLabel, TargetName, T)> {
        // FIXME: Would be better if we had a Display on self, so we could produce a nice error message.
        //        For now, just require the original string to be passed in for good errors.
        match self {
            ParsedPattern::Target(package, target_name, val) => Ok((package, target_name, val)),
            _ => Err(TargetPatternParseError::TargetLiteralRequired(original.to_owned()).into()),
        }
    }

    /// Parse a TargetPattern, but where there there is no relative directory.
    /// Generally, not a good thing to do - should aim to remove most of these.
    pub fn parse_precise(cell_resolver: &CellAliasResolver, pattern: &str) -> anyhow::Result<Self> {
        parse_target_pattern(
            cell_resolver,
            None,
            TargetParsingOptions::precise(),
            pattern,
        )
        .with_context(|| {
            format!(
                "Invalid absolute target pattern `{}` is not allowed",
                pattern
            )
        })
    }

    pub fn parsed_opt_absolute(
        cell_resolver: &CellAliasResolver,
        relative_dir: Option<CellPathRef>,
        pattern: &str,
    ) -> anyhow::Result<Self> {
        parse_target_pattern(
            cell_resolver,
            None,
            TargetParsingOptions {
                relative_dir,
                relative: false,
                infer_target: false,
                strip_package_trailing_slash: false,
            },
            pattern,
        )
        .with_context(|| {
            format!(
                "Invalid absolute target pattern `{}` is not allowed",
                pattern
            )
        })
    }

    /// Parse a TargetPattern out, resolving aliases via `cell_resolver`, and resolving relative
    /// targets via `enclosing_package`, if provided.
    /// Allows everything from `parse_absolute`, plus relative patterns.
    pub fn parse_relative(
        target_alias_resolver: &dyn TargetAliasResolver,
        cell_resolver: &CellAliasResolver,
        relative_dir: CellPathRef,
        pattern: &str,
    ) -> anyhow::Result<Self> {
        parse_target_pattern(
            cell_resolver,
            Some(target_alias_resolver),
            TargetParsingOptions {
                relative_dir: Some(relative_dir),
                relative: true,
                infer_target: false,
                strip_package_trailing_slash: false,
            },
            pattern,
        )
        .with_context(|| {
            format!(
                "Invalid relative target pattern `{}` is not allowed",
                pattern
            )
        })
    }

    /// Parse a TargetPattern out, resolving aliases via `cell_resolver`, resolving relative
    /// targets via `relative_dir`, inferring a target name if no target or recursive pattern
    /// is provided (e.g. `//foo/bar` is inferred to be equivalent to `//foo/bar:bar`), and
    /// stripping trailing `/` in package names instead of rejecting them.
    ///
    /// This should only be used with user-provided command line arguments, as precision is
    /// generally preferred elsewhere.
    pub fn parse_relaxed(
        target_alias_resolver: &dyn TargetAliasResolver,
        cell_resolver: &CellAliasResolver,
        relative_dir: CellPathRef,
        pattern: &str,
    ) -> anyhow::Result<Self> {
        parse_target_pattern(
            cell_resolver,
            Some(target_alias_resolver),
            TargetParsingOptions {
                relative_dir: Some(relative_dir),
                relative: true,
                infer_target: true,
                strip_package_trailing_slash: true,
            },
            pattern,
        )
        .with_context(|| {
            format!(
                "Invalid relative target pattern `{}` is not allowed",
                pattern
            )
        })
    }
}

impl<T: PatternType> Display for ParsedPattern<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParsedPattern::Target(package, target_name, pattern) => {
                write!(f, "{}:{}{}", package.as_cell_path(), target_name, pattern)
            }
            ParsedPattern::Package(package) => {
                write!(f, "{}:", package.as_cell_path())
            }
            ParsedPattern::Recursive(path) => {
                write!(f, "{}/...", path)
            }
        }
    }
}

#[derive(Debug)]
pub struct PatternParts<'a, T: PatternType> {
    /// Is there a `foo//` or `//` part.
    pub cell_alias: Option<&'a str>,
    pub pattern: PatternDataOrAmbiguous<'a, T>,
}

impl<'a> PatternParts<'a, ProvidersPatternExtra> {
    pub fn extra_try_into<T: PatternType>(self) -> anyhow::Result<PatternParts<'a, T>> {
        let PatternParts {
            cell_alias,
            pattern,
        } = self;
        Ok(PatternParts {
            cell_alias,
            pattern: pattern.extra_try_into()?,
        })
    }
}

#[derive(Debug, derive_more::From)]
pub enum PatternDataOrAmbiguous<'a, T: PatternType> {
    /// We successfully extracted PatternData.
    PatternData(PatternData<'a, T>),

    /// This pattern looks like `foo/bar`, `foo/bar/` or `foo`. It could be a package + target if
    /// we allow inference (i.e. expanding `foo/bar:bar`).
    Ambiguous {
        /// The pattern. If we allow inference this will become the package.
        pattern: &'a str,
        /// Whether we should strip trailing slashes out of this pattern before doing inference
        /// (rather than throwing an error).
        strip_package_trailing_slash: bool,
        extra: T,
    },
}

impl<'a> PatternDataOrAmbiguous<'a, ProvidersPatternExtra> {
    fn extra_try_into<T: PatternType>(self) -> anyhow::Result<PatternDataOrAmbiguous<'a, T>> {
        match self {
            PatternDataOrAmbiguous::PatternData(d) => {
                Ok(PatternDataOrAmbiguous::PatternData(d.extra_try_into()?))
            }
            PatternDataOrAmbiguous::Ambiguous {
                pattern,
                strip_package_trailing_slash,
                extra,
            } => Ok(PatternDataOrAmbiguous::Ambiguous {
                pattern,
                strip_package_trailing_slash,
                extra: T::from_parts(extra.providers)?,
            }),
        }
    }
}

impl<'a, T> PatternDataOrAmbiguous<'a, T>
where
    T: PatternType,
{
    /// If the pattern is ambiguous, try to infer a target. This would convert `foo/bar` into
    /// `foo/bar:bar`.
    pub fn infer_target(self) -> anyhow::Result<PatternData<'a, T>> {
        match self {
            Self::PatternData(d) => Ok(d),
            Self::Ambiguous {
                pattern,
                strip_package_trailing_slash,
                extra,
            } => {
                let package = normalize_package(pattern, strip_package_trailing_slash)?;

                let target = package
                    .file_name()
                    .context(TargetPatternParseError::PackageIsEmpty)?;

                let target_name = TargetName::new(target.as_ref())?;

                Ok(PatternData::TargetInPackage {
                    package,
                    target_name,
                    extra,
                })
            }
        }
    }

    /// If the pattern is ambiguous, error out.
    pub fn reject_ambiguity(self) -> anyhow::Result<PatternData<'a, T>> {
        match self {
            Self::PatternData(d) => Ok(d),
            Self::Ambiguous { pattern, .. } => {
                // Check if the user maybe tried to use a macro
                if pattern.contains('$')
                    && pattern.contains(' ')
                    && pattern.contains('(')
                    && pattern.contains(')')
                {
                    return Err(TargetPatternParseError::PossibleMacroUsage.into());
                }
                Err(TargetPatternParseError::UnexpectedFormat.into())
            }
        }
    }
}

impl<'a> PatternData<'a, ProvidersPatternExtra> {
    fn extra_try_into<T: PatternType>(self) -> anyhow::Result<PatternData<'a, T>> {
        match self {
            PatternData::Recursive { package } => Ok(PatternData::Recursive { package }),
            PatternData::AllTargetsInPackage { package } => {
                Ok(PatternData::AllTargetsInPackage { package })
            }
            PatternData::TargetInPackage {
                package,
                target_name,
                extra,
            } => Ok(PatternData::TargetInPackage {
                package,
                target_name,
                extra: T::from_parts(extra.providers)?,
            }),
        }
    }
}

/// The pattern data we extracted.
#[derive(Debug)]
pub enum PatternData<'a, T: PatternType> {
    /// A pattern like `foo/bar/...`.
    Recursive { package: &'a ForwardRelativePath },

    /// A pattern like `foo/bar:`, or `:`
    AllTargetsInPackage { package: &'a ForwardRelativePath },

    /// A pattern like `foo/bar:qux`, or `:qux`. The target will never be empty.
    TargetInPackage {
        package: &'a ForwardRelativePath,
        target_name: TargetName,
        extra: T,
    },
}

impl<'a, T: PatternType> PatternData<'a, T> {
    pub fn package_path(&self) -> &'a ForwardRelativePath {
        match self {
            Self::Recursive { package } => package,
            Self::AllTargetsInPackage { package } => package,
            Self::TargetInPackage { package, .. } => package,
        }
    }

    pub fn target(&self) -> Option<(&TargetName, &T)> {
        match self {
            Self::Recursive { .. } => None,
            Self::AllTargetsInPackage { .. } => None,
            Self::TargetInPackage {
                target_name, extra, ..
            } => Some((target_name, extra)),
        }
    }

    /// Whether this is a target that looks like `:target`.
    pub fn is_adjacent_target(&self) -> bool {
        self.package_path().is_empty() && self.target().is_some()
    }
}

// Splits a pattern into cell alias and forward relative path if "//" is present, otherwise returns None,
pub fn maybe_split_cell_alias_and_relative_path<'a>(
    pattern: &'a str,
) -> anyhow::Result<Option<(CellAlias, &'a ForwardRelativePath)>> {
    Ok(match split1_opt_ascii(pattern, AsciiStr2::new("//")) {
        Some((a, p)) => Some((
            CellAlias::new(trim_prefix_ascii(a, AsciiChar::new('@')).to_owned()),
            ForwardRelativePath::new(p)?,
        )),
        None => None,
    })
}

fn lex_provider_pattern<'a>(
    pattern: &'a str,
    strip_package_trailing_slash: bool,
) -> anyhow::Result<PatternParts<ProvidersPatternExtra>> {
    let (cell_alias, pattern) = match split1_opt_ascii(pattern, AsciiStr2::new("//")) {
        Some((a, p)) => (Some(trim_prefix_ascii(a, AsciiChar::new('@'))), p),
        None => (None, pattern),
    };

    let pattern = match split1_opt_ascii(pattern, AsciiChar::new(':')) {
        Some((package, "")) => PatternData::AllTargetsInPackage {
            package: normalize_package(package, strip_package_trailing_slash)?,
        }
        .into(),
        Some((package, target)) => {
            let (target, providers) = split_providers_name(target)?;
            let target_name = TargetName::new(target)?;
            let extra = ProvidersPatternExtra { providers };
            PatternData::TargetInPackage {
                package: normalize_package(package, strip_package_trailing_slash)?,
                target_name,
                extra,
            }
            .into()
        }
        None => {
            if let Some(package) = strip_suffix_ascii(pattern, AsciiStr::new("/...")) {
                PatternData::Recursive {
                    package: ForwardRelativePath::new(package)?,
                }
                .into()
            } else if pattern == "..." {
                PatternData::Recursive {
                    package: ForwardRelativePath::new("")?,
                }
                .into()
            } else if !pattern.is_empty() {
                let (pattern, providers) = split_providers_name(pattern)?;
                PatternDataOrAmbiguous::Ambiguous {
                    pattern,
                    strip_package_trailing_slash,
                    extra: ProvidersPatternExtra { providers },
                }
            } else {
                return Err(TargetPatternParseError::UnexpectedFormat.into());
            }
        }
    };

    Ok(PatternParts {
        cell_alias,
        pattern,
    })
}

// Lex the target pattern into the relevant pieces.
pub fn lex_target_pattern<'a, T: PatternType>(
    pattern: &'a str,
    strip_package_trailing_slash: bool,
) -> anyhow::Result<PatternParts<T>> {
    let provider_pattern = lex_provider_pattern(pattern, strip_package_trailing_slash)?;
    provider_pattern.extra_try_into().map_err(|_| {
        // This can only fail when `PatternType = TargetName`, so the message is correct.
        TargetPatternParseError::ExpectingTargetPatternWithoutProviders(pattern.to_owned()).into()
    })
}

fn normalize_package<'a>(
    package: &'a str,
    strip_package_trailing_slash: bool,
) -> anyhow::Result<&'a ForwardRelativePath> {
    // Strip or reject trailing `/`, such as in `foo/:bar`.
    if let Some(stripped) = strip_suffix_ascii(package, AsciiChar::new('/')) {
        if strip_package_trailing_slash {
            return ForwardRelativePath::new(stripped);
        } else {
            return Err(anyhow::Error::from(
                TargetPatternParseError::PackageTrailingSlash,
            ));
        }
    }

    ForwardRelativePath::new(package)
}

#[derive(Clone, Dupe)]
struct TargetParsingOptions<'a> {
    /// The dir this pattern should be intepreted relative to.  This will be used to prepend to the
    /// package if `relative` is set, otherwise it'll only be used for targets such as `:foo`.
    relative_dir: Option<CellPathRef<'a>>,
    /// Whether to interpret packages relatively.
    relative: bool,
    /// Whether to infer the target in a pattern such as `foo/bar` (to `foo/bar:bar`).
    infer_target: bool,
    /// Whether to strip trailing slashes in package names, in e.g. `foo/bar/` or `foo/bar/:qux`.
    /// If not set, trailing slashes are an error. Note that this happens before target inference
    /// (if enabled), so e.g. `foo/bar/` becomes `foo/bar:bar`.
    strip_package_trailing_slash: bool,
}

impl<'a> TargetParsingOptions<'a> {
    fn precise() -> TargetParsingOptions<'a> {
        TargetParsingOptions {
            relative_dir: None,
            relative: false,
            infer_target: false,
            strip_package_trailing_slash: false,
        }
    }
}

/// Parse a TargetPattern out, resolving aliases via `cell_resolver`, and resolving relative
/// targets via `enclosing_package`, if provided.
fn parse_target_pattern<T>(
    cell_resolver: &CellAliasResolver,
    target_alias_resolver: Option<&dyn TargetAliasResolver>,
    opts: TargetParsingOptions,
    pattern: &str,
) -> anyhow::Result<ParsedPattern<T>>
where
    T: PatternType,
{
    let TargetParsingOptions {
        relative_dir,
        relative,
        infer_target,
        strip_package_trailing_slash,
    } = opts;

    debug_assert!(if relative {
        relative_dir.is_some()
    } else {
        true
    });

    let lex = lex_target_pattern(pattern, strip_package_trailing_slash)?;

    if let Some(target_alias_resolver) = target_alias_resolver {
        if let Some(aliased) = resolve_target_alias(cell_resolver, target_alias_resolver, &lex)? {
            return Ok(aliased);
        }
    }

    let PatternParts {
        cell_alias,
        pattern,
    } = lex;

    let pattern = if infer_target {
        pattern.infer_target()?
    } else {
        pattern.reject_ambiguity()?
    };

    // This allows things of the form `//foo` (having a cell alias) or `:bar` (no cell, no package,
    // just relative target). This is a bit of a wonky  definition of "is_absolute" but we rely on
    // it.
    let is_absolute = cell_alias.is_some() || pattern.is_adjacent_target();
    if !relative && !is_absolute {
        return Err(TargetPatternParseError::AbsoluteRequired.into());
    }

    // We ask for the cell, but if the pattern is relative we might not use it
    let cell = cell_resolver.resolve(cell_alias.unwrap_or_default())?;

    let package_path = pattern.package_path();

    let path = match relative_dir {
        Some(rel) if cell_alias.is_none() && (relative || package_path.is_empty()) => {
            rel.join(package_path)
        }
        Some(rel)
            if rel.cell() == cell && rel.path().as_forward_relative_path() == package_path =>
        {
            rel.to_owned()
        }
        _ => CellPath::new(cell, CellRelativePath::new(package_path).to_owned()),
    };

    match pattern {
        PatternData::Recursive { .. } => Ok(ParsedPattern::Recursive(path)),
        PatternData::AllTargetsInPackage { .. } => Ok(ParsedPattern::Package(
            PackageLabel::from_cell_path(path.as_ref()),
        )),
        PatternData::TargetInPackage {
            target_name, extra, ..
        } => Ok(ParsedPattern::Target(
            PackageLabel::from_cell_path(path.as_ref()),
            target_name,
            extra,
        )),
    }
}

#[derive(thiserror::Error, Debug)]
enum ResolveTargetAliasError {
    #[error("Error dereferencing alias `{}` -> `{}`", target, alias)]
    ErrorDereferencing { target: String, alias: String },

    #[error("Invalid alias: `{}`", alias)]
    InvalidAlias { alias: String },

    #[error("Alias for `{}` is not a target: `{}`", target, alias)]
    AliasIsNotATarget { target: String, alias: String },
}

fn resolve_target_alias<T>(
    cell_resolver: &CellAliasResolver,
    target_alias_resolver: &dyn TargetAliasResolver,
    lex: &PatternParts<T>,
) -> anyhow::Result<Option<ParsedPattern<T>>>
where
    T: PatternType,
{
    // Imported from Buck1
    static ALIAS_REGEX: Lazy<Regex> =
        Lazy::new(|| Regex::new("^[a-zA-Z_-][a-zA-Z0-9_-]*$").unwrap());

    // If the input starts with a cell path, it can't be an alias.
    if lex.cell_alias.is_some() {
        return Ok(None);
    }

    // Unless the input is a standalone bit of ambiguous text then it cannot be an alias.
    let (target, extra) = match &lex.pattern {
        PatternDataOrAmbiguous::Ambiguous { pattern, extra, .. } => (*pattern, extra),
        _ => return Ok(None),
    };

    // Check if this is an alias after all.
    let alias = match target_alias_resolver.get(target)? {
        Some(alias) => alias,
        None => return Ok(None),
    };

    // Now that we know it's an alias, check it matches the regex. We only do this once we know the
    // alias is valid so that we avoid throwing "alias is invalid" if the user didn't mean to use
    // an alias.
    if !ALIAS_REGEX.is_match(target) {
        return Err(ResolveTargetAliasError::InvalidAlias {
            alias: alias.to_owned(),
        }
        .into());
    }

    // We found a matching alias. Parse the alias as a target.
    let res = parse_target_pattern::<TargetPatternExtra>(
        cell_resolver,
        None,
        TargetParsingOptions::precise(),
        alias,
    )
    .with_context(|| ResolveTargetAliasError::ErrorDereferencing {
        target: target.to_owned(),
        alias: alias.to_owned(),
    })?;

    // And finally, put the `T` we were looking for back together.
    let res = match res {
        ParsedPattern::Target(package, target_name, TargetPatternExtra) => {
            ParsedPattern::Target(package, target_name, extra.clone())
        }
        _ => {
            return Err(ResolveTargetAliasError::AliasIsNotATarget {
                target: target.to_owned(),
                alias: alias.to_owned(),
            }
            .into());
        }
    };

    Ok(Some(res))
}

#[derive(Debug, Eq, PartialEq)]
pub enum PackageSpec<T: PatternType> {
    /// Given targets in a package.
    Targets(Vec<(TargetName, T)>),
    /// All targets in a package, without subpackages.
    /// Syntax for this variant is `foo:`.
    All,
}

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use gazebo::prelude::*;
    use test_case::test_case;

    use super::*;
    use crate::cells::name::CellName;
    use crate::cells::paths::CellRelativePathBuf;
    use crate::cells::CellAlias;
    use crate::target::label::TargetLabel;
    use crate::target::name::TargetNameRef;

    fn mk_package<P: PatternType>(cell: &str, path: &str) -> ParsedPattern<P> {
        ParsedPattern::Package(PackageLabel::testing_new(cell, path))
    }

    fn mk_recursive<P: PatternType>(cell: &str, path: &str) -> ParsedPattern<P> {
        ParsedPattern::Recursive(CellPath::new(
            CellName::testing_new(cell),
            CellRelativePathBuf::unchecked_new(path.to_owned()),
        ))
    }

    fn mk_target(cell: &str, path: &str, target: &str) -> ParsedPattern<TargetPatternExtra> {
        ParsedPattern::Target(
            PackageLabel::testing_new(cell, path),
            TargetName::unchecked_new(target),
            TargetPatternExtra,
        )
    }

    fn mk_providers(
        cell: &str,
        path: &str,
        target: &str,
        providers: Option<&[&str]>,
    ) -> ParsedPattern<ProvidersPatternExtra> {
        ParsedPattern::Target(
            PackageLabel::testing_new(cell, path),
            TargetName::unchecked_new(target),
            ProvidersPatternExtra {
                providers: providers.map_or(ProvidersName::Default, |n| {
                    ProvidersName::NonDefault(Box::new(NonDefaultProvidersName::Named(
                        n.map(|s| ProviderName::new((*s).to_owned()).unwrap())
                            .into_boxed_slice(),
                    )))
                }),
            },
        )
    }

    fn fails<R>(x: anyhow::Result<R>, msgs: &[&str]) {
        match x {
            Err(e) => {
                let s = format!("{:#}", e);
                for msg in msgs {
                    if !s.contains(msg) {
                        panic!("Expected `{}` but missing from error `{:#}`", msg, e)
                    }
                }
            }
            Ok(_) => panic!("Expected failure but succeeded"),
        }
    }

    struct NoAliases;

    impl TargetAliasResolver for NoAliases {
        fn get<'r, 'a: 'r, 'b: 'r>(&'a self, _name: &'b str) -> anyhow::Result<Option<&'r str>> {
            Ok(None)
        }
    }

    fn aliases(aliases: &[(&str, &str)]) -> impl TargetAliasResolver {
        struct Aliases(Vec<(String, String)>);

        impl TargetAliasResolver for Aliases {
            fn get<'r, 'a: 'r, 'b: 'r>(&'a self, name: &'b str) -> anyhow::Result<Option<&'r str>> {
                Ok(self
                    .0
                    .iter()
                    .find(|(a, _)| *a == name)
                    .map(|(_, b)| b.as_str()))
            }
        }

        Aliases(
            aliases
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
        )
    }

    fn resolver() -> CellAliasResolver {
        let m = hashmap![
            CellAlias::new("cell1".to_owned()) => CellName::testing_new("cell1"),
            CellAlias::new("alias2".to_owned()) => CellName::testing_new("cell2"),
        ];
        CellAliasResolver::new(CellName::testing_new("root"), Arc::new(m)).expect("valid resolver")
    }

    #[test_case(PhantomData::< TargetPatternExtra >; "parsing TargetPattern")]
    #[test_case(PhantomData::< ProvidersPatternExtra >; "parsing ProvidersPattern")]
    fn parse_absolute_pattern<T: PatternType>(_: PhantomData<T>) {
        let package = CellPath::new(
            resolver().resolve_self(),
            CellRelativePath::unchecked_new("package/path").to_owned(),
        );

        assert_eq!(
            mk_package::<T>("root", "package/path"),
            ParsedPattern::<T>::parse_precise(&resolver(), "//package/path:").unwrap()
        );
        assert_eq!(
            mk_package::<T>("root", ""),
            ParsedPattern::<T>::parse_precise(&resolver(), "//:").unwrap()
        );
        assert_eq!(
            mk_package::<T>("cell1", "package/path"),
            ParsedPattern::<T>::parse_precise(&resolver(), "cell1//package/path:").unwrap()
        );
        assert_matches!(
            ParsedPattern::<T>::parse_precise(&resolver(), "package/path:"),
            Err(e) => {
                assert_matches!(
                    e.downcast_ref::<TargetPatternParseError>(),
                    Some(TargetPatternParseError::AbsoluteRequired)
                );
            }
        );
        assert_eq!(
            mk_package::<T>("cell2", "package/path"),
            ParsedPattern::<T>::parse_precise(&resolver(), "alias2//package/path:").unwrap()
        );
        assert_eq!(
            mk_package::<T>("cell2", "package/path"),
            ParsedPattern::<T>::parse_precise(&resolver(), "@alias2//package/path:").unwrap()
        );
        assert_eq!(
            mk_recursive::<T>("root", "package/path"),
            ParsedPattern::<T>::parse_precise(&resolver(), "//package/path/...").unwrap()
        );
        assert_eq!(
            mk_recursive::<T>("root", "package/path"),
            ParsedPattern::<T>::parse_relative(&NoAliases, &resolver(), package.as_ref(), "...")
                .unwrap()
        );
        assert_eq!(
            mk_recursive::<T>("root", "package/path/foo"),
            ParsedPattern::<T>::parse_relative(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "foo/..."
            )
            .unwrap()
        );
    }

    #[test]
    fn parse_relative_pattern() -> anyhow::Result<()> {
        let package = CellPath::new(
            resolver().resolve_self(),
            CellRelativePath::unchecked_new("package/path").to_owned(),
        );

        assert_eq!(
            mk_target("root", "package/path", "target"),
            ParsedPattern::parse_precise(&resolver(), "//package/path:target")?
        );
        assert_eq!(
            mk_target("root", "package/path/foo", "target"),
            ParsedPattern::parse_relative(&NoAliases, &resolver(), package.as_ref(), "foo:target")?
        );
        Ok(())
    }

    #[test]
    fn test_relaxed() -> anyhow::Result<()> {
        let package = CellPath::new(
            resolver().resolve_self(),
            CellRelativePath::unchecked_new("package").to_owned(),
        );

        assert_matches!(
            ParsedPattern::<TargetPatternExtra>::parse_relative(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "path"
            ),
            Err(e) => {
                assert_matches!(
                    e.downcast_ref::<TargetPatternParseError>(),
                    Some(TargetPatternParseError::UnexpectedFormat)
                );
            }
        );

        assert_eq!(
            mk_target("root", "package/path", "path"),
            ParsedPattern::parse_relaxed(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "//package/path"
            )?
        );
        assert_eq!(
            mk_target("root", "package/path", "path"),
            ParsedPattern::parse_relaxed(&NoAliases, &resolver(), package.as_ref(), "path")?
        );
        assert_eq!(
            mk_providers("root", "package/path", "path", Some(&["provider"])),
            ParsedPattern::parse_relaxed(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "path[provider]"
            )?
        );
        assert_eq!(
            mk_providers(
                "root",
                "package/path/subpath",
                "subpath",
                Some(&["provider"])
            ),
            ParsedPattern::parse_relaxed(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "path/subpath[provider]"
            )?
        );
        assert_eq!(
            mk_target("root", "package/path/subpath", "subpath"),
            ParsedPattern::parse_relaxed(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "path/subpath"
            )?
        );
        assert_eq!(
            mk_target("root", "package/path", "path"),
            ParsedPattern::parse_relaxed(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "//package/path/"
            )?
        );
        assert_eq!(
            mk_target("root", "package/path", "target"),
            ParsedPattern::parse_relaxed(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "//package/path/:target"
            )?
        );

        // Awkward but technically valid?
        assert_eq!(
            mk_target("root", "package", "foo"),
            ParsedPattern::parse_relaxed(&NoAliases, &resolver(), package.as_ref(), "/:foo")?
        );

        // There's no target here so this is invalid.
        assert_matches!(
            ParsedPattern::<TargetPatternExtra>::parse_relaxed(
                &NoAliases,
                &resolver(),
                package.as_ref(),
                "/"
            ),
            Err(e) => {
                assert_matches!(
                    e.downcast_ref::<TargetPatternParseError>(),
                    Some(TargetPatternParseError::PackageIsEmpty)
                );
            }
        );

        Ok(())
    }

    #[test]
    fn test_parsed_opt_absolute() -> anyhow::Result<()> {
        let package = CellPath::new(
            resolver().resolve_self(),
            CellRelativePath::unchecked_new("package/path").to_owned(),
        );

        assert_eq!(
            mk_target("root", "other", "target"),
            ParsedPattern::parsed_opt_absolute(
                &resolver(),
                Some(package.as_ref()),
                "//other:target"
            )?
        );
        assert_eq!(
            mk_target("root", "package/path", "target"),
            ParsedPattern::parsed_opt_absolute(&resolver(), Some(package.as_ref()), ":target")?
        );

        assert_matches!(
            ParsedPattern::<TargetPatternExtra>::parsed_opt_absolute(
                &resolver(),
                Some(package.as_ref()),
                "foo/bar"
            ),
            Err(e) => {
                assert_matches!(
                    e.downcast_ref::<TargetPatternParseError>(),
                    Some(TargetPatternParseError::UnexpectedFormat)
                );
            }
        );

        assert_matches!(
            ParsedPattern::<TargetPatternExtra>::parsed_opt_absolute(
                &resolver(),
                Some(package.as_ref()),
                "foo/bar:bar"
            ),
            Err(e) => {
                assert_matches!(
                    e.downcast_ref::<TargetPatternParseError>(),
                    Some(TargetPatternParseError::AbsoluteRequired)
                );
            }
        );

        Ok(())
    }

    #[test]
    fn test_aliases() -> anyhow::Result<()> {
        let package = CellPath::new(
            resolver().resolve_self(),
            CellRelativePath::unchecked_new("package").to_owned(),
        );

        let config = aliases(&[
            ("foo", "cell1//foo/bar:target"),
            ("invalid/alias", "cell1//foo/bar:target"),
            ("badalias", "cell1//foo/bar:"),
        ]);

        assert_eq!(
            mk_target("cell1", "foo/bar", "target"),
            ParsedPattern::parse_relaxed(&config, &resolver(), package.as_ref(), "foo")?
        );

        assert_matches!(
            ParsedPattern::<TargetPatternExtra>::parse_relaxed(
                &config,
                &resolver(),
                package.as_ref(),
                "invalid/alias"
            ),
            Err(e) => {
                assert_matches!(
                    e.downcast_ref::<ResolveTargetAliasError>(),
                    Some(ResolveTargetAliasError::InvalidAlias { .. })
                );
            }
        );

        assert_matches!(
            ParsedPattern::<TargetPatternExtra>::parse_relaxed(
                &config,
                &resolver(),
                package.as_ref(),
                "badalias"
            ),
            Err(e) => {
                assert_matches!(
                    e.downcast_ref::<ResolveTargetAliasError>(),
                    Some(ResolveTargetAliasError::AliasIsNotATarget { .. })
                );
            }
        );

        Ok(())
    }

    #[test]
    fn parse_providers_pattern() -> anyhow::Result<()> {
        assert_eq!(
            mk_providers("root", "package/path", "target", None),
            ParsedPattern::parse_precise(&resolver(), "//package/path:target")?
        );
        assert_eq!(
            mk_providers("root", "package/path", "target", Some(&["java-output"])),
            ParsedPattern::parse_precise(&resolver(), "//package/path:target[java-output]")?
        );
        assert_eq!(
            mk_providers(
                "root",
                "package/path",
                "target",
                Some(&["FDSIcon+FDSInternal.h"]),
            ),
            ParsedPattern::parse_precise(
                &resolver(),
                "//package/path:target[FDSIcon+FDSInternal.h]",
            )?
        );

        let (package, target_name, providers) =
            ParsedPattern::parse_precise(&resolver(), "//package/path:target#flavor")?
                .as_literal("")?;
        assert_eq!(
            "root//package/path:target#flavor",
            ProvidersPatternExtra::into_providers_label(providers, package, target_name.as_ref())
                .to_string(),
        );
        Ok(())
    }

    #[test]
    fn parse_providers_pattern_with_alias() -> anyhow::Result<()> {
        let package = CellPath::new(
            resolver().resolve_self(),
            CellRelativePath::unchecked_new("package").to_owned(),
        );

        let config = aliases(&[("foo", "cell1//foo/bar:target")]);

        assert_eq!(
            mk_providers("cell1", "foo/bar", "target", Some(&["qux"])),
            ParsedPattern::parse_relaxed(&config, &resolver(), package.as_ref(), "foo[qux]")?
        );

        Ok(())
    }

    #[test_case(PhantomData::< TargetPatternExtra >; "parsing TargetPattern")]
    #[test_case(PhantomData::< ProvidersPatternExtra >; "parsing ProvidersPattern")]
    fn parse_pattern_failure<T: PatternType>(_: PhantomData<T>) {
        fails(ParsedPattern::<T>::parse_precise(&resolver(), ""), &[]);
        fails(
            ParsedPattern::<T>::parse_precise(&resolver(), "//package/path"),
            &[],
        );
        fails(
            ParsedPattern::<T>::parse_precise(&resolver(), "//package..."),
            &[],
        );
        fails(
            ParsedPattern::<T>::parse_precise(&resolver(), "package"),
            &[],
        );
        fails(
            ParsedPattern::<T>::parse_precise(&resolver(), "bad_alias//package/path:"),
            &[
                "bad_alias//package/path:",
                "unknown cell alias: `bad_alias`.",
            ],
        );
        fails(
            ParsedPattern::<T>::parse_precise(&resolver(), "//package/path/:target"),
            &[],
        );
        fails(
            ParsedPattern::<T>::parse_precise(&resolver(), "//package/path/"),
            &[],
        );
        fails(
            ParsedPattern::<T>::parse_precise(&resolver(), "$(exe my macro)"),
            &[
                "$(exe my macro)",
                "You may be trying to use a macro instead of a target pattern. Macro usage is invalid here",
            ],
        );
    }

    #[test]
    fn bad_providers_label() {
        fails(
            ParsedPattern::<ProvidersPatternExtra>::parse_precise(
                &resolver(),
                "//package/path:target[unclosed",
            ),
            &[
                "//package/path:target[unclosed",
                "target pattern with `[` must end with `]` to mark end of providers set label",
            ],
        );
        fails(
            ParsedPattern::<ProvidersPatternExtra>::parse_precise(
                &resolver(),
                "//package/path:target[out]wrong",
            ),
            &[
                "//package/path:target[out]wrong",
                "target pattern with `[` must end with `]` to mark end of providers set label",
            ],
        );
        fails(
            ParsedPattern::<ProvidersPatternExtra>::parse_precise(&resolver(), "$(exe my macro)"),
            &[
                "$(exe my macro)",
                "You may be trying to use a macro instead of a target pattern. Macro usage is invalid here",
            ],
        );
    }

    #[test]
    fn parsed_pattern_contains() -> anyhow::Result<()> {
        let cell_resolver = resolver();

        let pkg1 = PackageLabel::new(
            cell_resolver.resolve_self(),
            CellRelativePath::unchecked_new("package/path"),
        );
        let pkg2 = PackageLabel::new(
            cell_resolver.resolve_self(),
            CellRelativePath::unchecked_new("package"),
        );
        let pkg3 = PackageLabel::new(
            cell_resolver.resolve_self(),
            CellRelativePath::unchecked_new("package2"),
        );
        let pkg_in_different_cell = PackageLabel::new(
            cell_resolver.resolve("cell1")?,
            CellRelativePath::unchecked_new("package/path"),
        );

        let target_in_pkg1 = TargetLabel::new(pkg1.dupe(), TargetNameRef::new("target")?);
        let another_target_in_pkg1 = TargetLabel::new(pkg1, TargetNameRef::new("target2")?);
        let target_in_pkg2 = TargetLabel::new(pkg2, TargetNameRef::new("target")?);
        let target_in_pkg3 = TargetLabel::new(pkg3, TargetNameRef::new("target")?);
        let target_in_different_cell =
            TargetLabel::new(pkg_in_different_cell, TargetNameRef::new("target")?);

        // Testing ParsedPattern::Target

        let pattern = ParsedPattern::parse_precise(&resolver(), "//package/path:target")?;
        assert!(pattern.matches(&target_in_pkg1));
        assert!(!pattern.matches(&another_target_in_pkg1));
        assert!(!pattern.matches(&target_in_pkg2));
        assert!(!pattern.matches(&target_in_pkg3));
        assert!(!pattern.matches(&target_in_different_cell));

        // Testing ParsedPattern::Package

        let pattern = ParsedPattern::parse_precise(&resolver(), "//package/path:")?;
        assert!(pattern.matches(&target_in_pkg1));
        assert!(pattern.matches(&another_target_in_pkg1));
        assert!(!pattern.matches(&target_in_pkg2));
        assert!(!pattern.matches(&target_in_pkg3));
        assert!(!pattern.matches(&target_in_different_cell));

        let pattern = ParsedPattern::parse_precise(&resolver(), "//package:")?;
        assert!(!pattern.matches(&target_in_pkg1));
        assert!(!pattern.matches(&another_target_in_pkg1));
        assert!(pattern.matches(&target_in_pkg2));
        assert!(!pattern.matches(&target_in_pkg3));
        assert!(!pattern.matches(&target_in_different_cell));

        // Testing ParsedPattern::Recursive

        let pattern = ParsedPattern::parse_precise(&resolver(), "//package/path/...")?;
        assert!(pattern.matches(&target_in_pkg1));
        assert!(pattern.matches(&another_target_in_pkg1));
        assert!(!pattern.matches(&target_in_pkg2));
        assert!(!pattern.matches(&target_in_pkg3));
        assert!(!pattern.matches(&target_in_different_cell));

        let pattern = ParsedPattern::parse_precise(&resolver(), "//package/...")?;
        assert!(pattern.matches(&target_in_pkg1));
        assert!(pattern.matches(&another_target_in_pkg1));
        assert!(pattern.matches(&target_in_pkg2));
        assert!(!pattern.matches(&target_in_pkg3));
        assert!(!pattern.matches(&target_in_different_cell));

        let pattern = ParsedPattern::parse_precise(&resolver(), "//...")?;
        assert!(pattern.matches(&target_in_pkg1));
        assert!(pattern.matches(&another_target_in_pkg1));
        assert!(pattern.matches(&target_in_pkg2));
        assert!(pattern.matches(&target_in_pkg3));
        assert!(!pattern.matches(&target_in_different_cell));

        let pattern = ParsedPattern::parse_precise(&resolver(), "cell1//...")?;
        assert!(!pattern.matches(&target_in_pkg1));
        assert!(!pattern.matches(&another_target_in_pkg1));
        assert!(!pattern.matches(&target_in_pkg2));
        assert!(!pattern.matches(&target_in_pkg3));
        assert!(pattern.matches(&target_in_different_cell));

        Ok(())
    }
}
