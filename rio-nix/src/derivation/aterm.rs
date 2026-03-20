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

        self.write_aterm_tail(&mut out);
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

        self.write_aterm_tail(&mut out);
        Ok(out)
    }
}

impl Derivation {
    /// Write the shared tail of a `Derive(...)` term: inputSrcs, platform,
    /// builder, args, env, and the closing `)`. Used by both [`Self::to_aterm`]
    /// and [`Self::to_aterm_modulo`] — the tail is byte-identical between them.
    fn write_aterm_tail(&self, out: &mut String) {
        // inputSrcs
        out.push('[');
        for (i, src) in self.input_srcs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(out, src);
        }
        out.push(']');
        out.push(',');

        // platform, builder
        write_aterm_string(out, &self.platform);
        out.push(',');
        write_aterm_string(out, &self.builder);
        out.push(',');

        // args
        out.push('[');
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(out, arg);
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
            write_aterm_string(out, key);
            out.push(',');
            write_aterm_string(out, value);
            out.push(')');
        }
        out.push(']');

        out.push(')'); // close Derive(
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_derivation() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-simple-test","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple-test"),("out","/nix/store/abc-simple-test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
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
        Ok(())
    }

    #[test]
    fn parse_multi_output_derivation() -> anyhow::Result<()> {
        let aterm = r#"Derive([("dev","/nix/store/abc-dev","",""),("lib","/nix/store/abc-lib","",""),("out","/nix/store/abc-out","","")],[],[],"x86_64-linux","/bin/sh",["-c","mkdir $out $dev $lib"],[("dev","/nix/store/abc-dev"),("lib","/nix/store/abc-lib"),("name","multi"),("out","/nix/store/abc-out"),("outputs","out dev lib"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        assert_eq!(drv.outputs().len(), 3);
        assert_eq!(drv.outputs()[0].name(), "dev");
        assert_eq!(drv.outputs()[1].name(), "lib");
        assert_eq!(drv.outputs()[2].name(), "out");
        assert_eq!(drv.env().get("outputs").unwrap(), "out dev lib");
        Ok(())
    }

    #[test]
    fn parse_fixed_output_derivation() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/abc-fixed"),("outputHash","abcdef0123456789"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].hash_algo(), "sha256");
        assert_eq!(drv.outputs()[0].hash(), "abcdef0123456789");
        assert!(drv.outputs()[0].is_fixed_output());
        Ok(())
    }

    #[test]
    fn parse_with_input_drvs() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-hello","","")],[("/nix/store/abc-bash.drv",["out"]),("/nix/store/abc-stdenv.drv",["out"])],["/nix/store/abc-source.sh"],"x86_64-linux","/nix/store/abc-bash/bin/bash",["-e","/nix/store/abc-source.sh"],[("name","hello"),("out","/nix/store/abc-hello"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        assert_eq!(drv.input_drvs().len(), 2);
        assert!(drv.input_drvs().contains_key("/nix/store/abc-bash.drv"));
        assert!(drv.input_drvs().contains_key("/nix/store/abc-stdenv.drv"));
        let bash_outputs = drv.input_drvs().get("/nix/store/abc-bash.drv").unwrap();
        assert!(bash_outputs.contains("out"));
        assert_eq!(drv.input_srcs().len(), 1);
        assert!(drv.input_srcs().contains("/nix/store/abc-source.sh"));
        Ok(())
    }

    #[test]
    fn parse_with_escaped_strings() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-escape","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo \"hello\\nworld\" > $out"],[("env_val","line1\nline2\ttab\nquote\"end"),("name","escape"),("out","/nix/store/abc-escape"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        assert_eq!(drv.args(), &["-c", "echo \"hello\\nworld\" > $out"]);
        assert_eq!(
            drv.env().get("env_val").unwrap(),
            "line1\nline2\ttab\nquote\"end"
        );
        Ok(())
    }

    #[test]
    fn parse_empty_env_value() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("empty",""),("name","test"),("out","/nix/store/abc-test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        assert_eq!(drv.env().get("empty").unwrap(), "");
        Ok(())
    }

    #[test]
    fn roundtrip_simple() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-simple","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple"),("out","/nix/store/abc-simple"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
        Ok(())
    }

    #[test]
    fn roundtrip_with_input_drvs() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-hello","","")],[("/nix/store/abc-bash.drv",["out"]),("/nix/store/abc-stdenv.drv",["out"])],["/nix/store/abc-source.sh"],"x86_64-linux","/nix/store/abc-bash/bin/bash",["-e","/nix/store/abc-source.sh"],[("name","hello"),("out","/nix/store/abc-hello"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
        Ok(())
    }

    #[test]
    fn roundtrip_with_escapes() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-escape","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo \"hello\\nworld\""],[("env","line1\nline2\ttab\nquote\"end"),("name","escape"),("out","/nix/store/abc-escape"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
        Ok(())
    }

    #[test]
    fn roundtrip_fixed_output() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/abc-fixed"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
        Ok(())
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
    fn parse_input_drvs_with_multiple_outputs() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/abc-multi.drv",["dev","lib","out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm)?;
        let multi_outputs = drv.input_drvs().get("/nix/store/abc-multi.drv").unwrap();
        assert_eq!(multi_outputs.len(), 3);
        assert!(multi_outputs.contains("dev"));
        assert!(multi_outputs.contains("lib"));
        assert!(multi_outputs.contains("out"));
        Ok(())
    }

    /// Parse a real `.drv` file from the local Nix store.
    #[test]
    fn parse_real_drv_file() -> anyhow::Result<()> {
        // Generate a simple derivation and parse it
        let output = std::process::Command::new("nix-instantiate")
            .args(["--expr", r#"derivation { name = "golden-test"; builder = "/bin/sh"; args = ["-c" "echo hello > $out"]; system = builtins.currentSystem; }"#])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("skipping parse_real_drv_file: nix-instantiate not available");
                return Ok(());
            }
        };

        let drv_path = String::from_utf8(output.stdout)?;
        let drv_path = drv_path.trim();

        let drv_content = std::fs::read_to_string(drv_path)?;
        let drv = Derivation::parse(&drv_content)?;

        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.platform(), "x86_64-linux");
        assert_eq!(drv.builder(), "/bin/sh");
        assert_eq!(drv.env().get("name").unwrap(), "golden-test");

        // Verify roundtrip: parse → serialize → parse
        let serialized = drv.to_aterm();
        let reparsed = Derivation::parse(&serialized)?;
        assert_eq!(drv, reparsed);
        Ok(())
    }

    /// Parse the real hello .drv (complex, many deps).
    #[test]
    fn parse_hello_drv() -> anyhow::Result<()> {
        let output = std::process::Command::new("nix-instantiate")
            .args(["<nixpkgs>", "-A", "hello"])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!(
                    "skipping parse_hello_drv: nix-instantiate not available or nixpkgs not found"
                );
                return Ok(());
            }
        };

        let drv_path = String::from_utf8(output.stdout)?;
        let drv_path = drv_path.trim();

        let drv_content = std::fs::read_to_string(drv_path)?;
        let drv = Derivation::parse(&drv_content)?;

        // hello has 1 output, multiple input drvs, and a rich env
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert!(!drv.input_drvs().is_empty());
        assert_eq!(drv.env().get("pname").unwrap(), "hello");
        assert_eq!(drv.platform(), "x86_64-linux");

        // Roundtrip
        let serialized = drv.to_aterm();
        let reparsed = Derivation::parse(&serialized)?;
        assert_eq!(drv, reparsed);
        Ok(())
    }

    /// Tests targeting specific cargo-mutants MISSED mutants (P0373).
    /// The fuzz target exercises parse-crash-freedom but NOT semantic
    /// correctness — mutations producing wrong-but-valid output slip
    /// through. These tests pin byte-level roundtrip and escape-char
    /// semantics against golden fixtures.
    mod mutants_gap {
        use super::*;

        /// Golden `.drv` with all five ATerm-escapable characters:
        /// `\\`, `\"`, `\n`, `\r`, `\t`. Exercises every escape branch in
        /// both `parse_string` and `write_aterm_string`.
        ///
        /// Inlined (not `include_str!`) to keep the fixture out of the
        /// crane fileset — adding a `rio-nix/tests/fixtures/` dir to the
        /// workspace src drv invalidates every downstream VM-test drv
        /// (all VM tests deploy rio-* binaries built from that src). A
        /// test-only string in `#[cfg(test)]` code does not.
        ///
        /// Raw string `r#"..."#` can't hold the ATerm escape sequences
        /// (it un-escapes nothing), so `concat!` builds the literal with
        /// the exact bytes a real `.drv` file would contain.
        const FIXTURE_ESCAPES: &str = concat!(
            r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-esc","","")]"#,
            r#",[],[],"x86_64-linux","/bin/sh",["-c","echo "#,
            // args[1] content: `echo "quoted" back\slash tab<TAB>here`
            // In ATerm the literals are escaped: \" \\  \t
            "\\\"quoted\\\" back\\\\slash tab\\there\"],",
            r#"[("multiline","line1"#,
            // env multiline value: `line1<LF>line2<CR>line3<TAB>after`
            // ATerm-escaped: \n \r \t
            "\\nline2\\rline3\\tafter\"),",
            r#"("name","escape-test"),"#,
            r#"("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-esc"),"#,
            r#"("system","x86_64-linux")])"#,
        );

        /// Golden `.drv` with 3 inputSrcs, 2 inputDrvs (one multi-output),
        /// and 3 args. Exercises the comma-separator arms of every list
        /// parser plus the `i > 0` separator checks in every serializer.
        const FIXTURE_MULTI_INPUT: &str = concat!(
            r#"Derive([("out","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-multi","","")]"#,
            r#",[("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep1.drv",["out"]),"#,
            r#"("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep2.drv",["dev","out"])]"#,
            r#",["/nix/store/aaa-src1","/nix/store/bbb-src2","/nix/store/ccc-src3"]"#,
            r#","x86_64-linux","/bin/sh",["-e","build.sh","arg2"]"#,
            r#",[("name","multi-input"),"#,
            r#"("out","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-multi"),"#,
            r#"("system","x86_64-linux")])"#,
        );

        fn fixture_escapes() -> &'static str {
            FIXTURE_ESCAPES
        }

        fn fixture_multi_input() -> &'static str {
            FIXTURE_MULTI_INPUT
        }

        /// Parse → serialize → byte-equal. Catches:
        /// - `pos += ch.len_utf8()` mutations (`+=` → `-=`/`*=` in advance)
        /// - escape-table mutations in `write_aterm_string`/`parse_string`
        /// - match-arm deletions in `parse_string`'s escape dispatcher
        ///
        /// All five escape sequences present; any wrong char → byte-diff.
        #[test]
        fn roundtrip_byte_equal_escapes() {
            let parsed = Derivation::parse(fixture_escapes()).unwrap();
            let reserialized = parsed.to_aterm();
            assert_eq!(
                reserialized,
                fixture_escapes(),
                "roundtrip diff: parser consumed wrong structure"
            );
        }

        /// Parse → serialize → byte-equal for the multi-element fixture.
        /// Catches the `i > 0` separator-comma mutations in `to_aterm` /
        /// `write_aterm_tail` (mutating `>` → `>=` emits a leading comma;
        /// `>` → `<` drops all interior commas). The fixture has ≥2 items
        /// in every list: outputs(1), inputDrvs(2), inputSrcs(3), args(3),
        /// env(3) — so every `i > 0` branch executes with i=0 AND i≥1.
        #[test]
        fn roundtrip_byte_equal_multi_input() {
            let parsed = Derivation::parse(fixture_multi_input()).unwrap();
            let reserialized = parsed.to_aterm();
            assert_eq!(
                reserialized,
                fixture_multi_input(),
                "roundtrip diff on multi-element fixture"
            );
        }

        /// Escape-char handling: `\\` → `\`, `\"` → `"`, `\n` → LF,
        /// `\r` → CR, `\t` → TAB. Mutation flipping any escape-table
        /// branch produces the wrong literal char. Roundtrip-byte-equal
        /// already catches this; this is the focused struct-field assert.
        #[test]
        fn escape_chars_roundtrip() {
            let parsed = Derivation::parse(fixture_escapes()).unwrap();
            // args[1] contains literal quote, backslash, tab (after escape)
            assert_eq!(
                parsed.args[1], "echo \"quoted\" back\\slash tab\there",
                "escape parsing broken: args={:?}",
                parsed.args
            );
            // env value contains literal LF, CR, TAB
            assert_eq!(
                parsed.env.get("multiline"),
                Some(&"line1\nline2\rline3\tafter".to_string()),
                "env escape parsing broken"
            );
        }

        /// Multi-element count: inputSrcs with 3 entries must parse as 3.
        /// Catches deleting the `Some(',')` match arm in `parse_string_list`
        /// (would error on non-empty list) and `==` → `!=` on the
        /// early-empty-bracket check (would return empty for non-empty
        /// input, or error for empty input).
        #[test]
        fn input_srcs_count_preserved() {
            let parsed = Derivation::parse(fixture_multi_input()).unwrap();
            assert_eq!(
                parsed.input_srcs.len(),
                3,
                "separator/count mutation: {:?}",
                parsed.input_srcs
            );
            assert!(parsed.input_srcs.contains("/nix/store/aaa-src1"));
            assert!(parsed.input_srcs.contains("/nix/store/bbb-src2"));
            assert!(parsed.input_srcs.contains("/nix/store/ccc-src3"));
            // Same mutation class applies to parse_input_drvs and the
            // nested output-names list.
            assert_eq!(parsed.input_drvs.len(), 2);
            assert_eq!(parsed.args.len(), 3);
            let dep2 = parsed
                .input_drvs
                .get("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep2.drv")
                .unwrap();
            assert_eq!(dep2.len(), 2, "nested string-list count");
        }

        /// `to_aterm_modulo` separator-comma mutations: the `i > 0` checks
        /// at the outputs/inputDrvs/output-names loops are on a separate
        /// code path from `to_aterm`. With identity rewrites and
        /// `mask_outputs=false`, modulo output equals plain to_aterm —
        /// any `>` mutation in those loops diverges.
        ///
        /// Targets MISSED mutants at aterm.rs:360/:391/:399 (confirmed by
        /// cargo-mutants run against sprint-1 @ 7ecc8b73).
        #[test]
        fn modulo_separator_commas() {
            let parsed = Derivation::parse(fixture_multi_input()).unwrap();
            // Identity rewrites — every inputDrv key maps to itself.
            let rewrites: BTreeMap<String, String> = parsed
                .input_drvs
                .keys()
                .map(|k| (k.clone(), k.clone()))
                .collect();
            let modulo = parsed.to_aterm_modulo(&rewrites, false).unwrap();
            // With identity rewrites + no masking, modulo == to_aterm.
            // A `>` → `>=` at :360/:391/:399 emits a leading comma in the
            // corresponding list → strings differ.
            assert_eq!(
                modulo,
                parsed.to_aterm(),
                "to_aterm_modulo(identity, false) should equal to_aterm"
            );
            // `mask_outputs=true`: output paths blanked. The `>` at :360
            // still gates the separator comma — masked output list must
            // not start with a comma.
            let masked = parsed.to_aterm_modulo(&rewrites, true).unwrap();
            assert!(
                masked.starts_with(r#"Derive([("out","","","")]"#),
                "mask_outputs=true: output path not blanked or leading \
                 comma: {}",
                &masked[..masked.len().min(60)]
            );
        }

        /// Multi-output masking: every output path blanked, no leading
        /// comma (catches `i > 0` at aterm.rs:360 for multi-output case).
        #[test]
        fn modulo_mask_multi_output() {
            let aterm = r#"Derive([("dev","/nix/store/aaa-dev","",""),("out","/nix/store/aaa-out","","")],[],[],"x86_64-linux","/bin/sh",[],[("name","x")])"#;
            let parsed = Derivation::parse(aterm).unwrap();
            let rewrites = BTreeMap::new();
            let masked = parsed.to_aterm_modulo(&rewrites, true).unwrap();
            // Two blanked-path outputs, comma-separated, no leading comma.
            assert!(
                masked.starts_with(r#"Derive([("dev","","",""),("out","","","")]"#),
                "multi-output mask: {}",
                &masked[..masked.len().min(80)]
            );
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// String strategy that includes all five ATerm-escaped characters.
        /// Weighted so ~10-15% of generated chars need escaping — exercises
        /// write_aterm_string's escape branches which the old regex-only
        /// strategy never hit.
        fn arb_aterm_string(max_len: usize) -> impl Strategy<Value = String> {
            proptest::collection::vec(
                prop_oneof![
                    // 85%: safe ASCII
                    30 => proptest::char::range('a', 'z'),
                    10 => proptest::char::range('0', '9'),
                    5  => Just(' '),
                    5  => Just('/'),
                    // 15%: escape-requiring chars
                    2  => Just('\\'),
                    2  => Just('"'),
                    2  => Just('\n'),
                    1  => Just('\r'),
                    1  => Just('\t'),
                ],
                0..max_len,
            )
            .prop_map(|chars| chars.into_iter().collect())
        }

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
                    DerivationOutput::new(name, path, hash_algo, hash)
                        .expect("generated to be valid")
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
                // args + env: include the five ATerm-escaped chars (\\, ", \n,
                // \r, \t) so the proptest actually exercises
                // write_aterm_string's escape branches. ~15% escape density.
                proptest::collection::vec(arb_aterm_string(20), 0..4),
                proptest::collection::btree_map("[a-zA-Z_]{1,10}", arb_aterm_string(30), 0..5),
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
                let reparsed = Derivation::parse(&serialized)?;
                prop_assert_eq!(drv, reparsed);
            }
        }
    }
}
