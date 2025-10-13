// SPDX-FileCopyrightText: 2024 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use yap::{IntoTokens, TokenLocation, Tokens};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, PartialEq, Eq)]
pub struct Error {
    pub pos: usize,
    pub kind: ErrorKind,
}

impl std::error::Error for Error {}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "aterm error at position {}: {}", self.pos, self.kind)
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ErrorKind {
    #[error("unexpected end of file")]
    Eof,
    #[error("expected: `{0}`")]
    Expected(String),
}

impl ErrorKind {
    fn at<L: TokenLocation>(self, pos: L) -> Error {
        Error {
            pos: pos.offset(),
            kind: self,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DerivationOutput {
    pub path: String,
    pub algo: String,
    pub hash: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DerivationInputDerivation {
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Derivation {
    pub outputs: BTreeMap<String, DerivationOutput>,
    pub input_drvs: BTreeMap<String, DerivationInputDerivation>,
    pub input_srcs: Vec<String>,
    pub system: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Derivation {
    pub fn parse(s: &str) -> Result<Self> {
        parse_derivation(&mut s.into_tokens())
    }
}

/// Returns an error if the next character is not c.
fn expect(t: &mut impl Tokens<Item = char>, c: char) -> Result<()> {
    if !t.token(c) {
        Err(ErrorKind::Expected(c.into()).at(t.location()))
    } else {
        Ok(())
    }
}

/// Returns an error if the next character(s) are not s.
fn expects(t: &mut impl Tokens<Item = char>, s: &str) -> Result<()> {
    if !t.tokens(s.chars()) {
        Err(ErrorKind::Expected(s.into()).at(t.location()))
    } else {
        Ok(())
    }
}

/// Returns the next character, or errors if there are no more.
fn expect_next(t: &mut impl Tokens<Item = char>) -> Result<char> {
    if let Some(c) = t.next() {
        Ok(c)
    } else {
        Err(ErrorKind::Eof.at(t.location()))
    }
}

/// Consumes either a ',' and returns false, or a ']' and returns true.
fn is_end_of_list(t: &mut impl Tokens<Item = char>) -> bool {
    match t.peek() {
        Some(',') => {
            t.next();
            false
        }
        Some(']') => {
            t.next();
            true
        }
        _ => false,
    }
}

fn parse_string(t: &mut impl Tokens<Item = char>) -> Result<String> {
    expect(t, '"')?;
    let mut s = String::new();
    loop {
        match expect_next(t)? {
            '"' => break Ok(s),
            '\\' => s.push(match expect_next(t)? {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                // The astute observer may notice that this eats the backslash from invalid
                // escape sequences, eg. \g -> g. This is bug compatibility with CppNix:
                // https://github.com/NixOS/nix/blob/2.19.3/src/libstore/derivations.cc
                c => c,
            }),
            c => s.push(c),
        }
    }
}

fn parse_strings(t: &mut impl Tokens<Item = char>) -> Result<Vec<String>> {
    expect(t, '[')?;
    let mut v = Vec::new();
    while !is_end_of_list(t) {
        v.push(parse_string(t)?);
    }
    Ok(v)
}

/// Parses a derivation.
///
/// Largely based on `parseDerivation()` (src/libstore/derivation.cc) from CppNix.
fn parse_derivation(t: &mut impl Tokens<Item = char>) -> Result<Derivation> {
    expects(t, "Derive(")?;

    // Outputs.
    let mut outputs = BTreeMap::new();
    expect(t, '[')?;
    while !is_end_of_list(t) {
        expect(t, '(')?;
        let id = parse_string(t)?;
        expect(t, ',')?;
        let path = parse_string(t)?;
        expect(t, ',')?;
        let algo = parse_string(t)?;
        expect(t, ',')?;
        let hash = parse_string(t)?;
        expect(t, ')')?;

        outputs.insert(id, DerivationOutput { path, algo, hash });
    }

    // Input derivations.
    let mut input_drvs = BTreeMap::new();
    expects(t, ",[")?;
    while !is_end_of_list(t) {
        expect(t, '(')?;
        let drv = parse_string(t)?;
        expect(t, ',')?;
        let outputs = parse_strings(t)?;
        expect(t, ')')?;

        input_drvs.insert(drv, DerivationInputDerivation { outputs });
    }

    // Smaller stuff.
    expect(t, ',')?;
    let input_srcs = parse_strings(t)?;
    expect(t, ',')?;
    let system = parse_string(t)?;
    expect(t, ',')?;
    let builder = parse_string(t)?;
    expect(t, ',')?;
    let args = parse_strings(t)?;

    // Environment variables.
    let mut env = BTreeMap::new();
    expects(t, ",[")?;
    while !is_end_of_list(t) {
        expect(t, '(')?;
        let name = parse_string(t)?;
        expect(t, ',')?;
        let value = parse_string(t)?;
        expect(t, ')')?;
        env.insert(name, value);
    }

    expect(t, ')')?;

    Ok(Derivation {
        outputs,
        input_drvs,
        input_srcs,
        system,
        builder,
        args,
        env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_derivation() {
        let data = "Derive([(\"out\",\"/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1\",\"\",\"\")],[(\"/nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26.drv\",[\"out\"]),(\"/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1.tar.gz.drv\",[\"out\"]),(\"/nix/store/ffffffffffffffffffffffffffffffff-stdenv-linux.drv\",[\"out\"])],[\"/nix/store/ffffffffffffffffffffffffffffffff-default-builder.sh\"],\"x86_64-linux\",\"/nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26/bin/bash\",[\"-e\",\"/nix/store/ffffffffffffffffffffffffffffffff-default-builder.sh\"],[(\"__structuredAttrs\",\"\"),(\"buildInputs\",\"\"),(\"builder\",\"/nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26/bin/bash\"),(\"cmakeFlags\",\"\"),(\"configureFlags\",\"\"),(\"depsBuildBuild\",\"\"),(\"depsBuildBuildPropagated\",\"\"),(\"depsBuildTarget\",\"\"),(\"depsBuildTargetPropagated\",\"\"),(\"depsHostHost\",\"\"),(\"depsHostHostPropagated\",\"\"),(\"depsTargetTarget\",\"\"),(\"depsTargetTargetPropagated\",\"\"),(\"doCheck\",\"1\"),(\"doInstallCheck\",\"1\"),(\"mesonFlags\",\"\"),(\"name\",\"hello-2.12.1\"),(\"nativeBuildInputs\",\"\"),(\"out\",\"/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1\"),(\"outputs\",\"out\"),(\"patches\",\"\"),(\"pname\",\"hello\"),(\"postInstallCheck\",\"stat \\\"${!outputBin}/bin/hello\\\"\n\"),(\"propagatedBuildInputs\",\"\"),(\"propagatedNativeBuildInputs\",\"\"),(\"src\",\"/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1.tar.gz\"),(\"stdenv\",\"/nix/store/ffffffffffffffffffffffffffffffff-stdenv-linux\"),(\"strictDeps\",\"\"),(\"system\",\"x86_64-linux\"),(\"version\",\"2.12.1\")])";
        assert_eq!(
            Derivation {
                outputs: BTreeMap::from_iter([(
                    "out".into(),
                    DerivationOutput {
                        path: "/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1".into(),
                        algo: String::new(),
                        hash: String::new(),
                    }
                )]),
                input_drvs: BTreeMap::from_iter([
                    (
                        "/nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26.drv".into(),
                        DerivationInputDerivation {
                            outputs: vec!["out".into()],
                        }
                    ),
                    (
                        "/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1.tar.gz.drv"
                            .into(),
                        DerivationInputDerivation {
                            outputs: vec!["out".into()],
                        }
                    ),
                    (
                        "/nix/store/ffffffffffffffffffffffffffffffff-stdenv-linux.drv".into(),
                        DerivationInputDerivation {
                            outputs: vec!["out".into()],
                        }
                    ),
                ]),
                input_srcs: vec![
                    "/nix/store/ffffffffffffffffffffffffffffffff-default-builder.sh".into()
                ],
                system: "x86_64-linux".into(),
                builder: "/nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26/bin/bash".into(),
                args: vec![
                    "-e".into(),
                    "/nix/store/ffffffffffffffffffffffffffffffff-default-builder.sh".into(),
                ],
                env: BTreeMap::from_iter([
                    ("__structuredAttrs".into(), "".into()),
                    ("buildInputs".into(), "".into()),
                    (
                        "builder".into(),
                        "/nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26/bin/bash".into()
                    ),
                    ("cmakeFlags".into(), "".into()),
                    ("configureFlags".into(), "".into()),
                    ("depsBuildBuild".into(), "".into()),
                    ("depsBuildBuildPropagated".into(), "".into()),
                    ("depsBuildTarget".into(), "".into()),
                    ("depsBuildTargetPropagated".into(), "".into()),
                    ("depsHostHost".into(), "".into()),
                    ("depsHostHostPropagated".into(), "".into()),
                    ("depsTargetTarget".into(), "".into()),
                    ("depsTargetTargetPropagated".into(), "".into()),
                    ("doCheck".into(), "1".into()),
                    ("doInstallCheck".into(), "1".into()),
                    ("mesonFlags".into(), "".into()),
                    ("name".into(), "hello-2.12.1".into()),
                    ("nativeBuildInputs".into(), "".into()),
                    (
                        "out".into(),
                        "/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1".into()
                    ),
                    ("outputs".into(), "out".into()),
                    ("patches".into(), "".into()),
                    ("pname".into(), "hello".into()),
                    (
                        "postInstallCheck".into(),
                        "stat \"${!outputBin}/bin/hello\"\n".into()
                    ),
                    ("propagatedBuildInputs".into(), "".into()),
                    ("propagatedNativeBuildInputs".into(), "".into()),
                    (
                        "src".into(),
                        "/nix/store/ffffffffffffffffffffffffffffffff-hello-2.12.1.tar.gz".into()
                    ),
                    (
                        "stdenv".into(),
                        "/nix/store/ffffffffffffffffffffffffffffffff-stdenv-linux".into()
                    ),
                    ("strictDeps".into(), "".into()),
                    ("system".into(), "x86_64-linux".into()),
                    ("version".into(), "2.12.1".into()),
                ]),
            },
            Derivation::parse(data).unwrap()
        );
    }

    #[test]
    fn test_parse_derivation_not_a_derivation() {
        assert_eq!(
            Err(Error {
                pos: 0,
                kind: ErrorKind::Expected("Derive(".into())
            }),
            Derivation::parse("Blorbo()")
        )
    }

    #[test]
    fn test_parse_string_empty() {
        assert_eq!("", &parse_string(&mut "\"\"".into_tokens()).unwrap());
    }
    #[test]
    fn test_parse_string() {
        assert_eq!(
            "hello world",
            &parse_string(&mut "\"hello world\"".into_tokens()).unwrap()
        );
    }
    #[test]
    fn test_parse_string_newline() {
        assert_eq!(
            "hello\nworld",
            &parse_string(&mut "\"hello\\nworld\"".into_tokens()).unwrap()
        );
    }
    #[test]
    fn test_parse_string_bad_escape() {
        assert_eq!(
            "helloworld",
            &parse_string(&mut "\"hello\\world\"".into_tokens()).unwrap()
        );
    }
    #[test]
    fn test_parse_string_unterminated() {
        assert_eq!(
            Error {
                pos: 12,
                kind: ErrorKind::Eof
            },
            parse_string(&mut "\"hello world".into_tokens()).unwrap_err()
        );
    }

    #[test]
    fn test_parse_strings() {
        assert_eq!(
            vec!["hello".to_string(), "world".to_string()],
            parse_strings(&mut "[\"hello\",\"world\"]".into_tokens()).unwrap()
        );
    }
}
