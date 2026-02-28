//! ATerm parsing and serialization for `Derivation`.

use std::collections::{BTreeMap, BTreeSet};

use super::{Derivation, DerivationError, DerivationOutput, MAX_ATERM_LIST_ITEMS};

// ---------------------------------------------------------------------------
// ATerm serialization helper
// ---------------------------------------------------------------------------

/// Write an ATerm-escaped string (with surrounding quotes) to the output.
fn write_aterm_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// ATerm parser
// ---------------------------------------------------------------------------

struct ATermParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> ATermParser<'a> {
    fn new(input: &'a str) -> Self {
        ATermParser { input, pos: 0 }
    }

    /// Peek at the current character without consuming.
    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    /// Advance past the current character.
    fn advance(&mut self) {
        if let Some(ch) = self.input[self.pos..].chars().next() {
            self.pos += ch.len_utf8();
        }
    }

    /// Consume the expected literal string, or return an error.
    fn expect(&mut self, expected: &str) -> Result<(), DerivationError> {
        if self.input[self.pos..].starts_with(expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            let remaining = &self.input[self.pos..];
            let got: String = remaining.chars().take(expected.len().max(10)).collect();
            Err(DerivationError::Expected {
                expected: expected.to_string(),
                got,
            })
        }
    }

    /// Parse an ATerm-quoted string: `"..."` with escape handling.
    fn parse_string(&mut self) -> Result<String, DerivationError> {
        if self.peek() != Some('"') {
            return Err(DerivationError::ExpectedStringStart);
        }
        self.advance(); // consume opening quote

        let mut result = String::new();
        loop {
            match self.peek() {
                None => return Err(DerivationError::UnexpectedEof),
                Some('"') => {
                    self.advance(); // consume closing quote
                    return Ok(result);
                }
                Some('\\') => {
                    self.advance(); // consume backslash
                    match self.peek() {
                        None => return Err(DerivationError::UnexpectedEof),
                        Some('n') => {
                            result.push('\n');
                            self.advance();
                        }
                        Some('r') => {
                            result.push('\r');
                            self.advance();
                        }
                        Some('t') => {
                            result.push('\t');
                            self.advance();
                        }
                        Some('\\') => {
                            result.push('\\');
                            self.advance();
                        }
                        Some('"') => {
                            result.push('"');
                            self.advance();
                        }
                        Some(c) => return Err(DerivationError::InvalidEscape(c)),
                    }
                }
                Some(c) => {
                    result.push(c);
                    self.advance();
                }
            }
        }
    }

    /// Parse a comma-separated list of strings inside `[...]`.
    fn parse_string_list(&mut self) -> Result<Vec<String>, DerivationError> {
        self.expect("[")?;
        let mut items = Vec::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(items);
        }
        loop {
            if items.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(items.len()));
            }
            items.push(self.parse_string()?);
            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(items);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse the outputs list: `[("name","path","hashAlgo","hash"), ...]`
    fn parse_outputs(&mut self) -> Result<Vec<DerivationOutput>, DerivationError> {
        self.expect("[")?;
        let mut outputs = Vec::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(outputs);
        }
        loop {
            if outputs.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(outputs.len()));
            }
            self.expect("(")?;
            let name = self.parse_string()?;
            self.expect(",")?;
            let path = self.parse_string()?;
            self.expect(",")?;
            let hash_algo = self.parse_string()?;
            self.expect(",")?;
            let hash = self.parse_string()?;
            self.expect(")")?;

            outputs.push(
                DerivationOutput::new(name, path, hash_algo, hash)
                    .map_err(|_| DerivationError::EmptyOutputName(outputs.len()))?,
            );

            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(outputs);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse inputDrvs: `[("drvPath",["out1","out2"]), ...]`
    fn parse_input_drvs(&mut self) -> Result<BTreeMap<String, BTreeSet<String>>, DerivationError> {
        self.expect("[")?;
        let mut drvs = BTreeMap::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(drvs);
        }
        loop {
            if drvs.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(drvs.len()));
            }
            self.expect("(")?;
            let drv_path = self.parse_string()?;
            self.expect(",")?;
            let output_names: BTreeSet<String> = self.parse_string_list()?.into_iter().collect();
            self.expect(")")?;

            drvs.insert(drv_path, output_names);

            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(drvs);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse environment: `[("key","value"), ...]`
    fn parse_env(&mut self) -> Result<BTreeMap<String, String>, DerivationError> {
        self.expect("[")?;
        let mut env = BTreeMap::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(env);
        }
        loop {
            if env.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(env.len()));
            }
            self.expect("(")?;
            let key = self.parse_string()?;
            self.expect(",")?;
            let value = self.parse_string()?;
            self.expect(")")?;

            env.insert(key, value);

            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(env);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse a full `Derive(...)` expression.
    fn parse_derivation(&mut self) -> Result<Derivation, DerivationError> {
        self.expect("Derive(")?;

        let outputs = self.parse_outputs()?;
        self.expect(",")?;

        let input_drvs = self.parse_input_drvs()?;
        self.expect(",")?;

        let input_srcs: BTreeSet<String> = self.parse_string_list()?.into_iter().collect();
        self.expect(",")?;

        let platform = self.parse_string()?;
        self.expect(",")?;

        let builder = self.parse_string()?;
        self.expect(",")?;

        let args = self.parse_string_list()?;
        self.expect(",")?;

        let env = self.parse_env()?;

        self.expect(")")?;

        Ok(Derivation {
            outputs,
            input_drvs,
            input_srcs,
            platform,
            builder,
            args,
            env,
        })
    }
}

impl Derivation {
    /// Parse a `.drv` file's ATerm content.
    pub fn parse(input: &str) -> Result<Self, DerivationError> {
        let mut parser = ATermParser::new(input);
        parser.parse_derivation()
    }

    /// Serialize back to ATerm format.
    pub fn to_aterm(&self) -> String {
        let mut out = String::new();
        out.push_str("Derive(");

        // outputs
        out.push('[');
        for (i, o) in self.outputs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, &o.name);
            out.push(',');
            write_aterm_string(&mut out, &o.path);
            out.push(',');
            write_aterm_string(&mut out, &o.hash_algo);
            out.push(',');
            write_aterm_string(&mut out, &o.hash);
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputDrvs
        out.push('[');
        for (i, (drv_path, output_names)) in self.input_drvs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, drv_path);
            out.push(',');
            out.push('[');
            for (j, name) in output_names.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                write_aterm_string(&mut out, name);
            }
            out.push(']');
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputSrcs
        out.push('[');
        for (i, src) in self.input_srcs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, src);
        }
        out.push(']');
        out.push(',');

        // platform, builder
        write_aterm_string(&mut out, &self.platform);
        out.push(',');
        write_aterm_string(&mut out, &self.builder);
        out.push(',');

        // args
        out.push('[');
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, arg);
        }
        out.push(']');
        out.push(',');

        // env
        out.push('[');
        for (i, (key, value)) in self.env.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, key);
            out.push(',');
            write_aterm_string(&mut out, value);
            out.push(')');
        }
        out.push(']');

        out.push(')');
        out
    }

    /// Serialize to ATerm with modified `inputDrvs` keys and optional output masking.
    ///
    /// Used by `hash_derivation_modulo` to produce the canonical ATerm for hashing.
    ///
    /// - `input_rewrites`: maps original drv path → replacement string (hex hash).
    ///   Every key in `self.input_drvs` must be present; missing keys return
    ///   `InputNotFound`.
    /// - `mask_outputs`: if true, output paths are replaced with `""` (for CA
    ///   floating / impure derivations).
    ///
    /// The `inputDrvs` section is re-sorted by the *replacement* keys (hex hashes),
    /// matching Nix C++ `Derivation::unparse` with `actualInputs`.
    pub fn to_aterm_modulo(
        &self,
        input_rewrites: &BTreeMap<String, String>,
        mask_outputs: bool,
    ) -> Result<String, DerivationError> {
        let mut out = String::new();
        out.push_str("Derive(");

        // outputs (optionally mask paths)
        out.push('[');
        for (i, o) in self.outputs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, &o.name);
            out.push(',');
            if mask_outputs {
                write_aterm_string(&mut out, "");
            } else {
                write_aterm_string(&mut out, &o.path);
            }
            out.push(',');
            write_aterm_string(&mut out, &o.hash_algo);
            out.push(',');
            write_aterm_string(&mut out, &o.hash);
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputDrvs -- re-sorted by replacement keys
        let mut replaced_inputs: BTreeMap<&str, &BTreeSet<String>> = BTreeMap::new();
        for (drv_path, output_names) in &self.input_drvs {
            let new_key = input_rewrites
                .get(drv_path)
                .ok_or_else(|| DerivationError::InputNotFound(drv_path.clone()))?;
            replaced_inputs.insert(new_key.as_str(), output_names);
        }

        out.push('[');
        for (i, (key, output_names)) in replaced_inputs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, key);
            out.push(',');
            out.push('[');
            for (j, name) in output_names.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                write_aterm_string(&mut out, name);
            }
            out.push(']');
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputSrcs, platform, builder, args, env -- identical to to_aterm()
        out.push('[');
        for (i, src) in self.input_srcs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, src);
        }
        out.push(']');
        out.push(',');

        write_aterm_string(&mut out, &self.platform);
        out.push(',');
        write_aterm_string(&mut out, &self.builder);
        out.push(',');

        out.push('[');
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, arg);
        }
        out.push(']');
        out.push(',');

        out.push('[');
        for (i, (key, value)) in self.env.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, key);
            out.push(',');
            write_aterm_string(&mut out, value);
            out.push(')');
        }
        out.push(']');

        out.push(')');
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_derivation() {
        let aterm = r#"Derive([("out","/nix/store/abc-simple-test","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple-test"),("out","/nix/store/abc-simple-test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.outputs()[0].path(), "/nix/store/abc-simple-test");
        assert_eq!(drv.outputs()[0].hash_algo(), "");
        assert_eq!(drv.outputs()[0].hash(), "");
        assert!(!drv.outputs()[0].is_fixed_output());
        assert!(drv.input_drvs().is_empty());
        assert!(drv.input_srcs().is_empty());
        assert_eq!(drv.platform(), "x86_64-linux");
        assert_eq!(drv.builder(), "/bin/sh");
        assert_eq!(drv.args(), &["-c", "echo hello > $out"]);
        assert_eq!(drv.env().get("name").unwrap(), "simple-test");
        assert_eq!(drv.env().get("system").unwrap(), "x86_64-linux");
    }

    #[test]
    fn parse_multi_output_derivation() {
        let aterm = r#"Derive([("dev","/nix/store/abc-dev","",""),("lib","/nix/store/abc-lib","",""),("out","/nix/store/abc-out","","")],[],[],"x86_64-linux","/bin/sh",["-c","mkdir $out $dev $lib"],[("dev","/nix/store/abc-dev"),("lib","/nix/store/abc-lib"),("name","multi"),("out","/nix/store/abc-out"),("outputs","out dev lib"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.outputs().len(), 3);
        assert_eq!(drv.outputs()[0].name(), "dev");
        assert_eq!(drv.outputs()[1].name(), "lib");
        assert_eq!(drv.outputs()[2].name(), "out");
        assert_eq!(drv.env().get("outputs").unwrap(), "out dev lib");
    }

    #[test]
    fn parse_fixed_output_derivation() {
        let aterm = r#"Derive([("out","/nix/store/abc-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/abc-fixed"),("outputHash","abcdef0123456789"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].hash_algo(), "sha256");
        assert_eq!(drv.outputs()[0].hash(), "abcdef0123456789");
        assert!(drv.outputs()[0].is_fixed_output());
    }

    #[test]
    fn parse_with_input_drvs() {
        let aterm = r#"Derive([("out","/nix/store/abc-hello","","")],[("/nix/store/abc-bash.drv",["out"]),("/nix/store/abc-stdenv.drv",["out"])],["/nix/store/abc-source.sh"],"x86_64-linux","/nix/store/abc-bash/bin/bash",["-e","/nix/store/abc-source.sh"],[("name","hello"),("out","/nix/store/abc-hello"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.input_drvs().len(), 2);
        assert!(drv.input_drvs().contains_key("/nix/store/abc-bash.drv"));
        assert!(drv.input_drvs().contains_key("/nix/store/abc-stdenv.drv"));
        let bash_outputs = drv.input_drvs().get("/nix/store/abc-bash.drv").unwrap();
        assert!(bash_outputs.contains("out"));
        assert_eq!(drv.input_srcs().len(), 1);
        assert!(drv.input_srcs().contains("/nix/store/abc-source.sh"));
    }

    #[test]
    fn parse_with_escaped_strings() {
        let aterm = r#"Derive([("out","/nix/store/abc-escape","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo \"hello\\nworld\" > $out"],[("env_val","line1\nline2\ttab\nquote\"end"),("name","escape"),("out","/nix/store/abc-escape"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.args(), &["-c", "echo \"hello\\nworld\" > $out"]);
        assert_eq!(
            drv.env().get("env_val").unwrap(),
            "line1\nline2\ttab\nquote\"end"
        );
    }

    #[test]
    fn parse_empty_env_value() {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("empty",""),("name","test"),("out","/nix/store/abc-test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.env().get("empty").unwrap(), "");
    }

    #[test]
    fn roundtrip_simple() {
        let aterm = r#"Derive([("out","/nix/store/abc-simple","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple"),("out","/nix/store/abc-simple"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

    #[test]
    fn roundtrip_with_input_drvs() {
        let aterm = r#"Derive([("out","/nix/store/abc-hello","","")],[("/nix/store/abc-bash.drv",["out"]),("/nix/store/abc-stdenv.drv",["out"])],["/nix/store/abc-source.sh"],"x86_64-linux","/nix/store/abc-bash/bin/bash",["-e","/nix/store/abc-source.sh"],[("name","hello"),("out","/nix/store/abc-hello"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

    #[test]
    fn roundtrip_with_escapes() {
        let aterm = r#"Derive([("out","/nix/store/abc-escape","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo \"hello\\nworld\""],[("env","line1\nline2\ttab\nquote\"end"),("name","escape"),("out","/nix/store/abc-escape"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

    #[test]
    fn roundtrip_fixed_output() {
        let aterm = r#"Derive([("out","/nix/store/abc-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/abc-fixed"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

    #[test]
    fn parse_rejects_invalid_prefix() {
        assert!(Derivation::parse("NotDerive([],...)").is_err());
    }

    #[test]
    fn parse_rejects_truncated_input() {
        assert!(Derivation::parse("Derive(").is_err());
        assert!(Derivation::parse("Derive([").is_err());
        assert!(Derivation::parse(r#"Derive([("out")"#).is_err());
    }

    #[test]
    fn parse_rejects_empty_output_name() {
        let aterm = r#"Derive([("","/nix/store/abc-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#;
        assert!(matches!(
            Derivation::parse(aterm),
            Err(DerivationError::EmptyOutputName(0))
        ));
    }

    #[test]
    fn parse_rejects_invalid_escape() {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("name","tes\x")])"#;
        assert!(matches!(
            Derivation::parse(aterm),
            Err(DerivationError::InvalidEscape('x'))
        ));
    }

    #[test]
    fn parse_input_drvs_with_multiple_outputs() {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/abc-multi.drv",["dev","lib","out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let multi_outputs = drv.input_drvs().get("/nix/store/abc-multi.drv").unwrap();
        assert_eq!(multi_outputs.len(), 3);
        assert!(multi_outputs.contains("dev"));
        assert!(multi_outputs.contains("lib"));
        assert!(multi_outputs.contains("out"));
    }

    /// Parse a real `.drv` file from the local Nix store.
    #[test]
    fn parse_real_drv_file() {
        // Generate a simple derivation and parse it
        let output = std::process::Command::new("nix-instantiate")
            .args(["--expr", r#"derivation { name = "golden-test"; builder = "/bin/sh"; args = ["-c" "echo hello > $out"]; system = builtins.currentSystem; }"#])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("skipping parse_real_drv_file: nix-instantiate not available");
                return;
            }
        };

        let drv_path = String::from_utf8(output.stdout).unwrap();
        let drv_path = drv_path.trim();

        let drv_content = std::fs::read_to_string(drv_path).unwrap();
        let drv = Derivation::parse(&drv_content).unwrap();

        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.platform(), "x86_64-linux");
        assert_eq!(drv.builder(), "/bin/sh");
        assert_eq!(drv.env().get("name").unwrap(), "golden-test");

        // Verify roundtrip: parse → serialize → parse
        let serialized = drv.to_aterm();
        let reparsed = Derivation::parse(&serialized).unwrap();
        assert_eq!(drv, reparsed);
    }

    /// Parse the real hello .drv (complex, many deps).
    #[test]
    fn parse_hello_drv() {
        let output = std::process::Command::new("nix-instantiate")
            .args(["<nixpkgs>", "-A", "hello"])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!(
                    "skipping parse_hello_drv: nix-instantiate not available or nixpkgs not found"
                );
                return;
            }
        };

        let drv_path = String::from_utf8(output.stdout).unwrap();
        let drv_path = drv_path.trim();

        let drv_content = std::fs::read_to_string(drv_path).unwrap();
        let drv = Derivation::parse(&drv_content).unwrap();

        // hello has 1 output, multiple input drvs, and a rich env
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert!(!drv.input_drvs().is_empty());
        assert_eq!(drv.env().get("pname").unwrap(), "hello");
        assert_eq!(drv.platform(), "x86_64-linux");

        // Roundtrip
        let serialized = drv.to_aterm();
        let reparsed = Derivation::parse(&serialized).unwrap();
        assert_eq!(drv, reparsed);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_output() -> impl Strategy<Value = DerivationOutput> {
            (
                "[a-z]{1,8}",                                                 // name
                "/nix/store/[a-z0-9]{32}-[a-z]{1,10}",                        // path
                prop_oneof![Just(String::new()), Just("sha256".to_string())], // hash_algo
            )
                .prop_map(|(name, path, hash_algo)| {
                    let hash = if hash_algo.is_empty() {
                        String::new()
                    } else {
                        "0".repeat(64) // 64-char hex digest
                    };
                    DerivationOutput::new(name, path, hash_algo, hash).unwrap()
                })
        }

        fn arb_derivation() -> impl Strategy<Value = Derivation> {
            (
                proptest::collection::vec(arb_output(), 1..4),
                proptest::collection::vec(
                    (
                        "/nix/store/[a-z0-9]{32}-[a-z]{1,8}\\.drv",
                        proptest::collection::btree_set("[a-z]{1,5}", 1..3),
                    ),
                    0..3,
                ),
                proptest::collection::btree_set("/nix/store/[a-z0-9]{32}-[a-z]{1,8}", 0..3),
                "(x86_64|aarch64)-linux",
                "/nix/store/[a-z0-9]{32}-bash/bin/bash",
                proptest::collection::vec("[a-zA-Z0-9 ./-]{0,20}", 0..4),
                proptest::collection::btree_map("[a-zA-Z_]{1,10}", "[a-zA-Z0-9 =/_.-]{0,30}", 0..5),
            )
                .prop_map(
                    |(outputs, input_drvs_vec, input_srcs, platform, builder, args, env)| {
                        let input_drvs: BTreeMap<String, BTreeSet<String>> =
                            input_drvs_vec.into_iter().collect();
                        Derivation {
                            outputs,
                            input_drvs,
                            input_srcs,
                            platform,
                            builder,
                            args,
                            env,
                        }
                    },
                )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn aterm_roundtrip(drv in arb_derivation()) {
                let serialized = drv.to_aterm();
                let reparsed = Derivation::parse(&serialized).unwrap();
                prop_assert_eq!(drv, reparsed);
            }
        }
    }
}
